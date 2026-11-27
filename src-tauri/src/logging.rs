//! Opt-in diagnostic logging ("debug mode").
//!
//! Verbosity is decided once at startup and never reconfigured while running:
//! no reload layer, the file sink is either constructed or not, and cleanup can
//! run before any appender exists (deleting an open file fails on Windows).
//!
//! The marker file is the single source of truth; toggling it takes effect on
//! the next launch.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, EnvFilter, Layer as _};

/// Presence of this file in the app config dir enables debug logging on next launch.
const MARKER_FILE: &str = "debug-logging";

const LOG_PREFIX: &str = "dashbeam-";
const LOG_SUFFIX: &str = ".log";

/// Roll-over size. Idle control-plane traffic alone is ~1.5 MB/day.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Total budget across all retained files.
const MAX_DIR_BYTES: u64 = 4 * 1024 * 1024;
/// How many files to keep, oldest discarded first.
const MAX_SESSION_FILES: usize = 3;

/// Debug mode expires after this long — chasing one bug shouldn't leave IP
/// addresses and file names recorded for months.
const MAX_ENABLED_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// Targeted rather than a blanket `debug`, which drowns the file in iroh
/// internals. These are lib target names, not package names — `sendme-protocol`
/// builds lib `protocol`, `sendme-native` builds lib `native`.
const DEBUG_FILTER: &str = "iroh::_events=debug,protocol=debug,native=debug,engine=debug,\
     dashbeam=debug,dashbeam_lib=debug";

/// Filter for the debug-mode log file: `DEBUG_FILTER` events, plus every
/// iroh *span*. iroh logs relay events from actor tasks under trace-level spans
/// (`endpoint{id=..}:relay-actor:..`); dropping those breaks the chain, and the
/// node, transfer and probe endpoints become indistinguishable.
fn debug_file_filter<S>() -> impl tracing_subscriber::layer::Filter<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::filter::{filter_fn, FilterExt as _};

    EnvFilter::new(DEBUG_FILTER).or(filter_fn(|meta| {
        meta.is_span() && (meta.target() == "iroh" || meta.target().starts_with("iroh::"))
    }))
}

/// `DefaultFields` under its own type. fmt layers cache formatted span fields
/// per formatter type, so sharing `DefaultFields` with the ANSI stdout layer
/// would write its escape codes into the log file.
#[derive(Default)]
struct PlainFields(fmt::format::DefaultFields);

impl<'writer> fmt::FormatFields<'writer> for PlainFields {
    fn format_fields<R: tracing_subscriber::field::RecordFields>(
        &self,
        writer: fmt::format::Writer<'writer>,
        fields: R,
    ) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

/// The debug-mode file sink: plain text, `debug_file_filter`.
fn debug_file_layer<S, W>(writer: W) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    fmt::layer()
        .fmt_fields(PlainFields::default())
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true)
        .with_filter(debug_file_filter())
}

/// Keeps the non-blocking writer's worker thread alive for the process lifetime.
/// Dropping this silently discards buffered lines.
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();
/// Whether the file sink was actually installed for *this* run.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Path of this session's log file, if any.
static SESSION_FILE: OnceLock<PathBuf> = OnceLock::new();

/// True when debug logging is writing to disk in this process.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// This session's log file, if debug logging is active.
pub fn session_file() -> Option<&'static Path> {
    SESSION_FILE.get().map(PathBuf::as_path)
}

pub fn marker_path(config_dir: &Path) -> PathBuf {
    config_dir.join(MARKER_FILE)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// When debug logging was switched on, if it is on. `None` for an older build's
/// marker, which held no timestamp and so never expires.
pub fn enabled_since(config_dir: &Path) -> Option<u64> {
    fs::read_to_string(marker_path(config_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn is_expired(since: u64) -> bool {
    now_secs().saturating_sub(since) > MAX_ENABLED_AGE_SECS
}

/// Whether debug logging is enabled for the *next* launch.
pub fn is_enabled(config_dir: &Path) -> bool {
    if !marker_path(config_dir).exists() {
        return false;
    }
    enabled_since(config_dir).is_none_or(|since| !is_expired(since))
}

/// Enable or disable debug logging from the next launch onwards.
///
/// The marker stores the moment it was enabled so the toggle can expire on its own.
pub fn set_enabled(config_dir: &Path, enabled: bool) -> io::Result<()> {
    let marker = marker_path(config_dir);
    if enabled {
        fs::create_dir_all(config_dir)?;
        fs::write(&marker, now_secs().to_string())
    } else {
        match fs::remove_file(&marker) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// Stdout-only subscriber for when the app directories can't be resolved —
/// otherwise the process would end up with no subscriber at all.
pub fn init_stdout_only() {
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(true)
                .with_line_number(true)
                .with_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                ),
        )
        .try_init();
}

/// Install the global subscriber. Call exactly once, as early as a log dir is
/// known. Any failure degrades to stdout-only. `try_init` rather than `init`
/// because both the desktop binary and the mobile library reach this.
pub fn init(config_dir: &Path, log_dir: &Path) {
    let stdout_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));

    let enabled = is_enabled(config_dir);

    // An expired marker is cleared here, so the toggle reads as off from now on.
    if !enabled && marker_path(config_dir).exists() {
        let _ = set_enabled(config_dir, false);
    }

    // Runs before any appender exists, so there is never an open handle to unlink.
    if let Err(error) = prune(log_dir, enabled) {
        eprintln!("debug logging: could not prune old logs: {error}");
    }

    let file_layer = if enabled {
        match RollingWriter::new(log_dir.to_path_buf(), MAX_FILE_BYTES) {
            Ok((writer, path)) => {
                let (non_blocking, guard) = tracing_appender::non_blocking(writer);
                let _ = GUARD.set(guard);
                let _ = SESSION_FILE.set(path);
                ACTIVE.store(true, Ordering::Relaxed);
                Some(debug_file_layer(non_blocking))
            }
            Err(error) => {
                eprintln!("debug logging: could not open log file: {error}");
                None
            }
        }
    } else {
        None
    };

    // `Option<Layer>` is itself a Layer: `None` means no file handle, no I/O.
    if tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
        .try_init()
        .is_err()
    {
        // Already initialised (e.g. a test harness). Not fatal.
        return;
    }

    if is_active() {
        tracing::debug!(
            target: "dashbeam::_events::debug_mode",
            file = ?session_file(),
            "debug logging active"
        );
    }
}

/// Nanosecond stamp so a rollover cannot collide with the file it just closed.
fn create_log_file(log_dir: &Path) -> io::Result<(fs::File, PathBuf)> {
    fs::create_dir_all(log_dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = log_dir.join(format!("{LOG_PREFIX}{stamp}{LOG_SUFFIX}"));
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    Ok((file, path))
}

fn is_log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(LOG_PREFIX) && n.ends_with(LOG_SUFFIX))
}

/// Collect session logs, newest first.
fn session_logs(log_dir: &Path) -> io::Result<Vec<(PathBuf, u64)>> {
    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !is_log_file(&path) {
            continue;
        }
        let meta = entry.metadata()?;
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        files.push((path, meta.len(), modified));
    }
    files.sort_by(|a, b| b.2.cmp(&a.2));
    Ok(files.into_iter().map(|(p, len, _)| (p, len)).collect())
}

/// Delete everything when disabled; otherwise keep the newest files within both caps.
fn prune(log_dir: &Path, enabled: bool) -> io::Result<()> {
    let files = session_logs(log_dir)?;
    if !enabled {
        for (path, _) in files {
            let _ = fs::remove_file(path);
        }
        return Ok(());
    }

    let mut total = 0u64;
    for (index, (path, len)) in files.into_iter().enumerate() {
        total = total.saturating_add(len);
        if index >= MAX_SESSION_FILES || total > MAX_DIR_BYTES {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

/// Remove every session log, including the current one where the OS allows it.
pub fn clear(log_dir: &Path) -> io::Result<()> {
    for (path, _) in session_logs(log_dir)? {
        // The active file is still open on Windows; it is removed at next startup.
        let _ = fs::remove_file(path);
    }
    Ok(())
}

/// Concatenated session logs, newest last, truncated to `max_bytes` keeping the tail.
pub fn read_logs(log_dir: &Path, max_bytes: usize) -> io::Result<String> {
    let mut files = session_logs(log_dir)?;
    files.reverse(); // oldest first, so the newest lines end up at the tail
    let mut out = String::new();
    for (path, _) in files {
        if let Ok(text) = fs::read_to_string(&path) {
            out.push_str(&text);
        }
    }
    if out.len() > max_bytes {
        // Keep the most recent output and start at a line boundary.
        let cut = out.len() - max_bytes;
        let start = out[cut..].find('\n').map_or(cut, |i| cut + i + 1);
        out = format!("[… {cut} earlier bytes truncated …]\n{}", &out[start..]);
    }
    Ok(out)
}

/// Rolls to a fresh file at the cap, keeping the most recent output — stopping
/// at the cap would preserve Monday and never record Friday's bug.
struct RollingWriter {
    dir: PathBuf,
    file: fs::File,
    written: u64,
    cap: u64,
}

impl RollingWriter {
    fn new(dir: PathBuf, cap: u64) -> io::Result<(Self, PathBuf)> {
        let (file, path) = create_log_file(&dir)?;
        Ok((
            Self {
                dir,
                file,
                written: 0,
                cap,
            },
            path,
        ))
    }

    fn roll(&mut self) -> io::Result<()> {
        let (file, _) = create_log_file(&self.dir)?;
        self.file = file;
        self.written = 0;
        // Startup pruning alone would let a long-running session grow unbounded.
        let _ = prune(&self.dir, true);
        Ok(())
    }
}

impl Write for RollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // `self.written > 0` guarantees progress even if a single line exceeds the cap.
        if self.written > 0 && self.written.saturating_add(buf.len() as u64) > self.cap {
            self.roll()?;
        }
        let n = self.file.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod filter_tests {
    use super::DEBUG_FILTER;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use tracing_subscriber::{EnvFilter, Layer};

    /// Counts events that survive the filter.
    struct Hits(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> Layer<S> for Hits {
        fn on_event(&self, _event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A macro, not a loop: `tracing` builds a `static` callsite from the target,
    /// so it must be a literal.
    macro_rules! assert_admitted {
        ($hits:expr, $($target:literal),+ $(,)?) => {
            $({
                let before = $hits.load(Ordering::Relaxed);
                tracing::debug!(target: $target, "filter probe");
                assert!(
                    $hits.load(Ordering::Relaxed) > before,
                    "DEBUG_FILTER drops {}",
                    $target,
                );
            })+
        };
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// iroh emits relay events from actor tasks under
    /// `endpoint{id=..}:relay-actor:...`, where the inner spans are trace level.
    /// If the filter drops those, the chain breaks and every endpoint (node,
    /// transfer, probe) looks alike — a self-hoster can't tell which one is
    /// talking to public relays.
    #[test]
    fn debug_file_filter_keeps_endpoint_id_on_iroh_actor_events() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber =
            tracing_subscriber::registry().with(super::debug_file_layer(move || writer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let endpoint =
                tracing::info_span!(target: "iroh::endpoint", "endpoint", id = %"abc123");
            let actor = tracing::trace_span!(
                target: "iroh::socket::transports::relay::actor",
                parent: &endpoint,
                "relay-actor"
            );
            actor.in_scope(|| {
                tracing::debug!(target: "iroh::_events::relay::connected", url = "https://relay.example");
                tracing::trace!(target: "iroh::socket", "internal noise");
            });
        });

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let line = output
            .lines()
            .find(|line| line.contains("iroh::_events::relay::connected"))
            .expect("relay event should be logged");
        assert!(
            line.contains("endpoint{id=abc123}"),
            "missing endpoint id: {line}"
        );
        assert!(
            !output.contains("internal noise"),
            "iroh internals leaked: {output}"
        );
    }

    /// fmt layers cache formatted span fields per field-formatter type, so the
    /// ANSI stdout layer must not colour the span fields the file writes.
    #[test]
    fn debug_file_layer_stays_plain_text_beside_ansi_stdout() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_ansi(true),
            )
            .with(super::debug_file_layer(move || writer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let _endpoint =
                tracing::info_span!(target: "iroh::endpoint", "endpoint", id = %"abc123").entered();
            tracing::debug!(target: "iroh::_events::relay::connected", url = "https://relay.example");
        });

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("endpoint{id=abc123}"),
            "unexpected output: {output:?}"
        );
        assert!(
            !output.contains('\u{1b}'),
            "ANSI escapes in log file: {output:?}"
        );
    }

    /// A target the filter drops is silently missing from bug-report logs, with
    /// nothing at the call site to show it.
    #[test]
    fn debug_filter_admits_every_diagnostic_target() {
        let hits = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry()
            .with(Hits(hits.clone()).with_filter(EnvFilter::new(DEBUG_FILTER)));

        tracing::subscriber::with_default(subscriber, || {
            assert_admitted!(
                hits,
                // Pairing.
                "dashbeam::_events::pairing::host_open",
                "dashbeam::_events::pairing::join_attempt",
                "dashbeam::_events::pairing::join_failed",
                "dashbeam::_events::pairing::invite_sent",
                "dashbeam::_events::pairing::invite_response",
                "dashbeam::_events::pairing::forget",
                "dashbeam::_events::pairing::nearby_accept",
                "dashbeam::_events::pairing::nearby_commit",
                "dashbeam::_events::pairing::remote_unpair",
                // Nearby / mDNS.
                "dashbeam::_events::nearby::mdns_advertising",
                "dashbeam::_events::nearby::mdns_discovered",
                "dashbeam::_events::nearby::mdns_expired",
                "dashbeam::_events::nearby::mdns_unavailable",
                "dashbeam::_events::nearby::observe",
                "dashbeam::_events::nearby::identity",
                "dashbeam::_events::nearby::expire",
                "dashbeam::_events::nearby::probe_started",
                "dashbeam::_events::nearby::probe_failed",
                "dashbeam::_events::nearby::discoverability",
                // Control plane, shared by pairing and nearby.
                "dashbeam::_events::control::msg_in",
                "dashbeam::_events::control::msg_out",
                "dashbeam::_events::control::rejected_unpaired",
            );
        });
    }
}
