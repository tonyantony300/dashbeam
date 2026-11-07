//! `dashbeam` subcommands. Everything but `up` is a socket client.

use crate::daemon::{self, EXIT_ALREADY_RUNNING};
use crate::ipc::{self, Client, Request, StartedBy, StatusInfo, WaitResult};
use crate::prompt::{display_safe as safe, display_safe_multiline, format_bytes, TtyApprover};
use agent::approver::{Approver, DenyAll};
use agent::config::AgentConfig;
use agent::inbound::PendingInvite;
use agent::lock::NodeLock;
use agent::messages::Message;
use agent::registry::{Direction, TransferRecord, TransferStatus};
use agent::{AgentOptions, AgentService};
use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use engine::{DeviceInfo, PairedDeviceInfo};
use serde::Serialize;
use serde_json::Value;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default lifetime of `pair --host`.
const DEFAULT_PAIR_HOST_TTL: u64 = 300;

#[derive(Debug, Parser)]
#[command(
    name = "dashbeam",
    version,
    about = "DashBeam agent node: send and receive files between paired devices"
)]
pub struct Cli {
    /// Agent data dir (identity, config, socket). Default: $DASHBEAM_AGENT_DIR,
    /// then the OS data dir's `dashbeam-agent`.
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Run the node in the foreground until Ctrl-C.
    Up(UpArgs),
    /// Stop the running node.
    Stop,
    /// Show whether a node is running, and how it was started.
    Status,
    /// Rename this device.
    Name { name: String },
    /// Print this node's endpoint id (for another node's `pairing.authorized`).
    Id,
    /// List paired devices.
    Devices,
    /// List devices on the local network.
    Nearby,
    /// Pair by code (`--host` / `--join`) or with a nearby device.
    Pair(PairArgs),
    /// Auto-accept files from a paired device.
    Trust { device: String },
    /// Ask before accepting files from a paired device.
    Untrust { device: String },
    /// Unpair a device.
    Forget { device: String },
    /// Send files to a paired device.
    Send {
        device: String,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Wait until the transfer finishes.
        #[arg(long)]
        wait: bool,
    },
    /// Share files behind a one-time ticket (prints the ticket).
    Share {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Ticket lifetime in seconds (default: config `ticket_ttl_secs`).
        #[arg(long, value_name = "S")]
        ttl: Option<u64>,
    },
    /// Download a ticket into the receive dir.
    Receive {
        ticket: String,
        /// Wait until the download finishes.
        #[arg(long)]
        wait: bool,
    },
    /// List recent transfers.
    Transfers {
        #[arg(long, value_enum)]
        direction: Option<DirectionArg>,
    },
    /// Pending invites from paired-but-untrusted devices.
    Inbox {
        #[command(subcommand)]
        action: Option<InboxAction>,
    },
    /// Cancel a queued or running transfer.
    Cancel { transfer_id: String },
    /// Text messages with paired devices.
    #[command(visible_alias = "message")]
    Msg {
        #[command(subcommand)]
        action: MsgAction,
    },
    /// Serve MCP over stdio for AI agents (auto-starts the node).
    Mcp,
}

#[derive(Debug, Args)]
pub struct UpArgs {
    /// Display name (overrides config `name`).
    #[arg(long, value_name = "N")]
    pub name: Option<String>,
    /// Be visible to everyone on the LAN (discoverability `everyone`).
    #[arg(long)]
    pub discoverable: bool,
    /// Let agents (MCP) send nearby pair requests, trust or untrust paired
    /// devices, and accept pending invites from unpaired nearby devices
    /// (which pairs them). Nothing is accepted automatically; incoming pair
    /// requests still need `pairing.authorized` or a terminal yes. Never a
    /// config key.
    #[arg(long)]
    pub dangerously_allow_agent_pairing: bool,
    /// Started by a client's auto-start; logs go to the data dir's node.log.
    #[arg(long, hide = true)]
    pub background: bool,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("mode").required(true).args(["host", "join", "device"])))]
pub struct PairArgs {
    /// Open pairing and print a code for the other device to join.
    #[arg(long)]
    pub host: bool,
    /// Join the device that printed CODE.
    #[arg(long, value_name = "CODE")]
    pub join: Option<String>,
    /// Send a pair request to this nearby device.
    pub device: Option<String>,
    /// How long `--host` stays open, in seconds.
    #[arg(long, value_name = "S", requires = "host")]
    pub ttl: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum InboxAction {
    /// Accept a pending invite.
    Accept { invite_id: String },
    /// Decline a pending invite.
    Decline { invite_id: String },
}

#[derive(Debug, Subcommand)]
pub enum MsgAction {
    /// Send a text message to a paired device.
    Send {
        device: String,
        /// The message; several words are joined with spaces.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Wait for unread messages and print them (each is handed out once).
    Wait {
        /// Seconds to wait (max 120).
        #[arg(long, value_name = "S", default_value_t = 60)]
        timeout: u64,
    },
    /// List recent messages, sent and received.
    List,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DirectionArg {
    In,
    Out,
}

/// Runs the parsed command; returns the process exit code.
pub async fn run(cli: Cli, iroh_secret_was_set: bool) -> Result<i32> {
    let json = cli.json;
    // Validate send/share paths before touching the node, so a typo fails fast.
    let local_paths = match &cli.command {
        Cmd::Send { paths, .. } | Cmd::Share { paths, .. } => Some(absolutize(paths)?),
        _ => None,
    };
    let dir = agent::paths::resolve_data_dir(cli.data_dir)?;
    match cli.command {
        Cmd::Up(args) => up(&dir, args, iroh_secret_was_set).await,
        Cmd::Stop => stop(&dir).await,
        Cmd::Status => status(&dir, json).await,
        Cmd::Name { name } => {
            let me: DeviceInfo = node(&dir).await?.call(Request::Rename { name }).await?;
            if let Some(cfg) = config_name(&dir) {
                eprintln!(
                    "dashbeam: warning: config.toml sets name = \"{}\", which replaces this name the next time the node starts",
                    safe(&cfg)
                );
            }
            emit(json, &me, || println!("{}", me.display_name))
        }
        Cmd::Id => {
            let me: DeviceInfo = node(&dir).await?.call(Request::Identity).await?;
            emit(json, &me, || println!("{}", me.endpoint_id))
        }
        Cmd::Devices => {
            let devices: Vec<PairedDeviceInfo> =
                node(&dir).await?.call(Request::ListDevices).await?;
            emit(json, &devices, || print_devices(&devices))
        }
        Cmd::Nearby => {
            let nearby: Vec<Value> = node(&dir).await?.call(Request::ListNearby).await?;
            emit(json, &nearby, || print_nearby(&nearby))
        }
        Cmd::Pair(args) => pair(&dir, args, json).await,
        Cmd::Trust { device } => trust(&dir, device, true, json).await,
        Cmd::Untrust { device } => trust(&dir, device, false, json).await,
        Cmd::Forget { device } => {
            let _: Value = node(&dir)
                .await?
                .call(Request::Forget {
                    device: device.clone(),
                })
                .await?;
            emit(json, &Value::Null, || println!("forgot {device}"))
        }
        Cmd::Send { device, wait, .. } => {
            let (paths, cwd) = local_paths.expect("validated above");
            let mut c = node(&dir).await?;
            let rec: TransferRecord = c.call(Request::SendToDevice { device, paths, cwd }).await?;
            finish_transfer(&mut c, rec, wait, json, TransferStatus::Delivered).await
        }
        Cmd::Share { ttl, .. } => {
            let (paths, cwd) = local_paths.expect("validated above");
            let rec: TransferRecord = node(&dir)
                .await?
                .call(Request::ShareTicket {
                    paths,
                    cwd,
                    ttl_secs: ttl,
                })
                .await?;
            emit(json, &rec, || {
                if let Some(exp) = rec.expires_at_ms {
                    let secs = exp.saturating_sub(engine::unix_now_ms()) / 1000;
                    eprintln!(
                        "one-time ticket for {} (transfer {}), expires in {secs}s; anyone holding it can download once:",
                        safe(&rec.file_names.join(", ")),
                        rec.id
                    );
                }
                println!("{}", rec.ticket.as_deref().unwrap_or_default());
            })
        }
        Cmd::Receive { ticket, wait } => {
            let mut c = node(&dir).await?;
            let rec: TransferRecord = c.call(Request::ReceiveTicket { ticket }).await?;
            finish_transfer(&mut c, rec, wait, json, TransferStatus::Received).await
        }
        Cmd::Transfers { direction } => {
            let direction = direction.map(|d| match d {
                DirectionArg::In => Direction::Inbound,
                DirectionArg::Out => Direction::Outbound,
            });
            let list: Vec<TransferRecord> = node(&dir)
                .await?
                .call(Request::ListTransfers {
                    after_seq: None,
                    direction,
                })
                .await?;
            emit(json, &list, || print_transfers(&list))
        }
        Cmd::Inbox { action } => inbox(&dir, action, json).await,
        Cmd::Msg { action } => msg(&dir, action, json).await,
        Cmd::Mcp => {
            // stdout carries the protocol; logs go to stderr.
            init_tracing(true);
            crate::mcp::serve_stdio(&dir).await?;
            Ok(0)
        }
        Cmd::Cancel { transfer_id } => {
            let rec: TransferRecord = node(&dir)
                .await?
                .call(Request::CancelTransfer { id: transfer_id })
                .await?;
            emit(json, &rec, || {
                println!("{} {}", rec.id, status_str(rec.status))
            })
        }
    }
}

async fn node(dir: &Path) -> Result<Client> {
    daemon::ensure_node(dir).await
}

fn emit<T: Serialize>(json: bool, value: &T, human: impl FnOnce()) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        human();
    }
    Ok(0)
}

/// The `name` config.toml sets, if any; it wins over `dashbeam name` on next start.
fn config_name(dir: &Path) -> Option<String> {
    AgentConfig::load(dir)
        .ok()?
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// Absolute paths against this process's cwd (never the daemon's); each must exist.
pub(crate) fn absolutize(paths: &[PathBuf]) -> Result<(Vec<PathBuf>, PathBuf)> {
    let cwd = std::env::current_dir().context("read the current directory")?;
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let abs = std::path::absolute(cwd.join(p))
            .with_context(|| format!("resolve path {}", p.display()))?;
        if std::fs::metadata(&abs).is_err() {
            bail!("path not found: {}", abs.display());
        }
        out.push(abs);
    }
    Ok((out, cwd))
}

fn status_str(s: TransferStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{s:?}"))
}

async fn finish_transfer(
    c: &mut Client,
    rec: TransferRecord,
    wait: bool,
    json: bool,
    success: TransferStatus,
) -> Result<i32> {
    if !wait {
        emit(json, &rec, || {
            println!("{} {}", rec.id, status_str(rec.status));
            if let Some(e) = &rec.error {
                eprintln!("dashbeam: {}", display_safe_multiline(e));
            }
        })?;
        // Already finished without success (e.g. a bad ticket): say so in the exit code.
        let failed = rec.status.is_terminal() && rec.status != success;
        return Ok(if failed { 1 } else { 0 });
    }
    if !json {
        eprintln!("transfer {}: {}", rec.id, status_str(rec.status));
    }
    let id = rec.id.clone();
    let rec = loop {
        let w: WaitResult = c
            .call(Request::WaitTransfer {
                id: id.clone(),
                timeout_secs: ipc::MAX_WAIT_SECS,
            })
            .await?;
        if !w.still_running {
            break w.record;
        }
    };
    emit(json, &rec, || {
        println!("{}", status_str(rec.status));
        for p in &rec.saved_paths {
            println!("{}", safe(&p.display().to_string()));
        }
        if let Some(e) = &rec.error {
            eprintln!("dashbeam: {}", display_safe_multiline(e));
        }
    })?;
    Ok(if rec.status == success { 0 } else { 1 })
}

// ---------------------------------------------------------------- up / stop / status

fn init_tracing(background: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,agent=info,dashbeam=info,dashbeam_cli=info"));
    let ansi = !background && std::io::stderr().is_terminal();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .try_init();
}

async fn up(dir: &Path, args: UpArgs, iroh_secret_was_set: bool) -> Result<i32> {
    init_tracing(args.background);
    ipc::check_socket_path(dir)?;
    let lock = match NodeLock::acquire(dir) {
        Ok(lock) => lock,
        Err(e) if e.to_string().contains("already running") => {
            eprintln!("dashbeam: a node is already running for {}", dir.display());
            return Ok(EXIT_ALREADY_RUNNING);
        }
        Err(e) => return Err(e),
    };
    if iroh_secret_was_set {
        tracing::warn!("IROH_SECRET is set but ignored by the dashbeam agent");
    }
    let mut config = AgentConfig::load(dir)?;
    if let Some(name) = args.name {
        config.name = Some(name);
    }
    let interactive = !args.background && std::io::stdin().is_terminal();
    let approver: Arc<dyn Approver> = if interactive {
        Arc::new(TtyApprover::new(Duration::from_secs(
            config.invite_timeout_secs,
        )))
    } else {
        Arc::new(DenyAll)
    };
    let started_by = if args.background {
        StartedBy::Auto
    } else {
        StartedBy::Hand
    };
    let svc = AgentService::start(AgentOptions {
        data_dir: dir.to_path_buf(),
        config,
        allow_agent_pairing: args.dangerously_allow_agent_pairing,
        discoverable_override: args.discoverable,
        approver,
    })
    .await?;
    let server = match ipc::Server::bind(svc.clone(), started_by) {
        Ok(s) => s,
        Err(e) => {
            let _ = svc.shutdown().await;
            return Err(e);
        }
    };
    let me = svc.identity();
    tracing::info!(
        socket = %server.socket_path().display(),
        interactive,
        "dashbeam node \"{}\" is up ({})",
        me.display_name,
        me.endpoint_id
    );
    let served = server.run(shutdown_signal()).await;
    tracing::info!("shutting down");
    let stopped = svc.shutdown().await;
    drop(lock);
    served?;
    stopped?;
    Ok(0)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        async fn on(kind: SignalKind) {
            match signal(kind) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = on(SignalKind::terminate()) => {}
            _ = on(SignalKind::hangup()) => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        let mut brk = ctrl_break().ok();
        let mut close = ctrl_close().ok();
        let mut shut = ctrl_shutdown().ok();
        // Ctrl-Break, closing the console window, or system shutdown. A branch
        // whose handler failed to register just never fires.
        tokio::select! {
            Ok(()) = tokio::signal::ctrl_c() => {}
            Some(()) = async { brk.as_mut()?.recv().await } => {}
            Some(()) = async { close.as_mut()?.recv().await } => {}
            Some(()) = async { shut.as_mut()?.recv().await } => {}
            else => std::future::pending::<()>().await,
        }
    }
}

async fn stop(dir: &Path) -> Result<i32> {
    let Some(stopping) = ipc::request_stop(dir).await? else {
        eprintln!("no dashbeam node is running for {}", dir.display());
        return Ok(0);
    };
    // Wait for the process to exit so a following `up` or auto-start can't race it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let gone = match stopping.pid {
            Some(pid) => !process_alive(pid),
            None => NodeLock::acquire(dir).is_ok(),
        };
        if gone {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the dashbeam node did not exit within 30s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("stopped");
    Ok(0)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 only checks existence; EPERM still means it exists.
    // SAFETY: kill(2) with signal 0 sends nothing.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        let _ = CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

async fn status(dir: &Path, json: bool) -> Result<i32> {
    let mut c = match Client::connect(dir).await {
        Ok(c) => c,
        Err(e) if ipc::is_not_running(&e) => {
            eprintln!("no dashbeam node is running for {}", dir.display());
            return Ok(1);
        }
        Err(e) => return Err(e),
    };
    let st: StatusInfo = c.call(Request::Status).await?;
    emit(json, &st, || {
        let how = match st.info.started_by {
            StartedBy::Hand => "by hand",
            StartedBy::Auto => "automatically",
        };
        println!(
            "running (pid {}, started {how}, up {}s)",
            st.info.pid, st.uptime_secs
        );
        println!("  name       {}", st.info.node.display_name);
        println!("  id         {}", st.info.node.endpoint_id);
        println!("  data dir   {}", st.info.data_dir.display());
        println!("  receive    {}", st.receive_dir.display());
        println!(
            "  devices    {} paired, {} online",
            st.paired_devices, st.online_devices
        );
        println!(
            "  transfers  {} active, {} pending invite(s)",
            st.active_transfers, st.pending_invites
        );
        println!("  messages   {} unread", st.unread_messages);
        if st.info.agent_pairing_allowed {
            println!("  agent pairing ALLOWED (--dangerously-allow-agent-pairing)");
        }
    })
}

// ---------------------------------------------------------------- pairing

async fn pair(dir: &Path, args: PairArgs, json: bool) -> Result<i32> {
    let mut c = node(dir).await?;
    let before: Vec<PairedDeviceInfo> = c.call(Request::ListDevices).await?;
    if args.host {
        let ttl = args.ttl.unwrap_or(DEFAULT_PAIR_HOST_TTL);
        let code: String = c
            .call(Request::HostPairing {
                ttl_secs: Some(ttl),
            })
            .await?;
        if json {
            println!("{}", serde_json::to_string(&code)?);
        } else {
            eprintln!(
                "pairing open for {ttl}s; on the other device run `dashbeam pair --join <code>` or enter it in the app:"
            );
            println!("{code}");
            eprintln!("waiting for a device to pair (Ctrl-C to stop waiting)…");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let now: Vec<PairedDeviceInfo> = c.call(Request::ListDevices).await?;
            let new = newly_paired(&before, &now);
            if !new.is_empty() {
                if !json {
                    for d in new {
                        println!(
                            "paired with {} ({})",
                            safe(&d.display_name),
                            safe(&d.endpoint_id)
                        );
                    }
                }
                return Ok(0);
            }
        }
        eprintln!("dashbeam: pairing code expired with no device paired");
        return Ok(1);
    }
    if let Some(code) = args.join {
        let _: Value = c.call(Request::Join { code }).await?;
        let now: Vec<PairedDeviceInfo> = c.call(Request::ListDevices).await?;
        let new = newly_paired(&before, &now);
        return emit(json, &new, || {
            if new.is_empty() {
                println!("paired");
            }
            for d in &new {
                println!(
                    "paired with {} ({})",
                    safe(&d.display_name),
                    safe(&d.endpoint_id)
                );
            }
        });
    }
    let device = args.device.expect("clap requires one mode");
    let delivered: bool = c
        .call(Request::PairNearby {
            device: device.clone(),
        })
        .await?;
    if json {
        println!("{delivered}");
    } else if delivered {
        println!("pair request sent to {device}; accept it on that device");
    }
    if !delivered {
        eprintln!("dashbeam: the pair request could not be delivered to {device}");
        return Ok(1);
    }
    Ok(0)
}

fn newly_paired<'a>(
    before: &[PairedDeviceInfo],
    now: &'a [PairedDeviceInfo],
) -> Vec<&'a PairedDeviceInfo> {
    now.iter()
        .filter(|d| d.pairing_status.is_active())
        .filter(|d| {
            !before
                .iter()
                .any(|b| b.endpoint_id == d.endpoint_id && b.pairing_status.is_active())
        })
        .collect()
}

async fn trust(dir: &Path, device: String, trusted: bool, json: bool) -> Result<i32> {
    let d: PairedDeviceInfo = node(dir)
        .await?
        .call(Request::Trust { device, trusted })
        .await?;
    emit(json, &d, || {
        let what = if d.trusted { "trusted" } else { "untrusted" };
        println!(
            "{} ({}) is now {what}",
            safe(&d.display_name),
            safe(&d.endpoint_id)
        );
    })
}

async fn inbox(dir: &Path, action: Option<InboxAction>, json: bool) -> Result<i32> {
    let mut c = node(dir).await?;
    match action {
        None => {
            let pending: Vec<PendingInvite> = c.call(Request::ListIncoming).await?;
            emit(json, &pending, || {
                if pending.is_empty() {
                    println!("no pending invites");
                }
                for p in &pending {
                    println!(
                        "{}  {} ({})  {} file(s), {}",
                        p.invite_id,
                        safe(&p.sender_name),
                        crate::prompt::short_fingerprint(&p.endpoint_id),
                        p.file_count,
                        format_bytes(p.total_size)
                    );
                }
            })
        }
        Some(InboxAction::Accept { invite_id }) => {
            let rec: TransferRecord = c.call(Request::AcceptIncoming { invite_id }).await?;
            emit(json, &rec, || {
                println!("{} {}", rec.id, status_str(rec.status))
            })
        }
        Some(InboxAction::Decline { invite_id }) => {
            let _: Value = c.call(Request::DeclineIncoming { invite_id }).await?;
            emit(json, &Value::Null, || println!("declined"))
        }
    }
}

async fn msg(dir: &Path, action: MsgAction, json: bool) -> Result<i32> {
    let mut c = node(dir).await?;
    match action {
        MsgAction::Send { device, text } => {
            let m: Message = c
                .call(Request::SendMessage {
                    device,
                    text: text.join(" "),
                })
                .await?;
            emit(json, &m, || println!("sent to {}", safe(&peer_label(&m))))
        }
        MsgAction::Wait { timeout } => {
            let list: Vec<Message> = c
                .call(Request::WaitMessages {
                    timeout_secs: timeout.min(ipc::MAX_WAIT_SECS),
                })
                .await?;
            emit(json, &list, || print_messages(&list))?;
            // 2 = nothing arrived, so scripts can loop on it.
            Ok(if list.is_empty() { 2 } else { 0 })
        }
        MsgAction::List => {
            let list: Vec<Message> = c.call(Request::ListMessages { after_seq: None }).await?;
            emit(json, &list, || print_messages(&list))
        }
    }
}

fn peer_label(m: &Message) -> String {
    m.peer
        .name
        .clone()
        .or_else(|| m.sender_name.clone())
        .or_else(|| m.peer.endpoint_id.clone())
        .unwrap_or_else(|| "-".into())
}

// ---------------------------------------------------------------- printing

fn print_messages(list: &[Message]) {
    if list.is_empty() {
        println!("no messages");
    }
    for m in list {
        let arrow = match m.direction {
            Direction::Outbound => "->",
            Direction::Inbound => "<-",
        };
        println!(
            "[{}] {arrow} {}:\n{}\n",
            m.seq,
            safe(&peer_label(m)),
            display_safe_multiline(&m.text)
        );
    }
}

fn print_devices(devices: &[PairedDeviceInfo]) {
    if devices.is_empty() {
        println!("no paired devices");
    }
    for d in devices {
        let online = if d.online { "online" } else { "offline" };
        let trusted = if d.trusted { "trusted" } else { "untrusted" };
        let status = if d.pairing_status.is_active() {
            String::new()
        } else {
            format!("  ({:?})", d.pairing_status)
        };
        println!(
            "{}  [{}]  {online}  {trusted}  {}{status}",
            safe(&d.display_name),
            safe(&d.device_type),
            safe(&d.endpoint_id)
        );
    }
}

fn print_nearby(nearby: &[Value]) {
    if nearby.is_empty() {
        println!("no nearby devices");
    }
    for d in nearby {
        let s = |k: &str| safe(d[k].as_str().unwrap_or("?"));
        let name = safe(d["displayName"].as_str().unwrap_or("(unidentified)"));
        println!(
            "{name}  [{}]  {}  {}",
            s("deviceType"),
            s("fingerprint"),
            s("endpointId")
        );
    }
}

fn print_transfers(list: &[TransferRecord]) {
    if list.is_empty() {
        println!("no transfers");
    }
    for r in list {
        let dir = match r.direction {
            Direction::Outbound => "out",
            Direction::Inbound => "in ",
        };
        let peer = r
            .peer
            .as_ref()
            .and_then(|p| p.name.clone().or(p.endpoint_id.clone()))
            .map(|p| safe(&p))
            .unwrap_or_else(|| "-".into());
        println!(
            "{}  {dir}  {:<20}  {:<16}  {}  ({})",
            r.id,
            status_str(r.status),
            peer,
            safe(&r.file_names.join(", ")),
            format_bytes(r.total_bytes)
        );
    }
}
