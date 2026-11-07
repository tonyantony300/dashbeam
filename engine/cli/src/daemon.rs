//! Auto-starting the node: connect if one is running, else spawn a detached
//! `dashbeam up --background` and wait for its socket.

use crate::ipc::{self, Client};
use agent::lock::NodeLock;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const POLL: Duration = Duration::from_millis(250);
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// `up` exit code when the data dir's node lock is already held.
pub const EXIT_ALREADY_RUNNING: i32 = 2;

/// Returns a client for the node serving `data_dir`, starting one in the
/// background if none is running. A stale socket (node killed) is removed
/// only when the node lock proves no node owns it.
pub async fn ensure_node(data_dir: &Path) -> Result<Client> {
    match Client::connect(data_dir).await {
        Ok(c) => return Ok(c),
        Err(e) if ipc::is_not_running(&e) => {}
        Err(e) => return Err(e),
    }

    let mut child = spawn_if_unlocked(data_dir)?;
    let log = agent::paths::log_path(data_dir);
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        tokio::time::sleep(POLL).await;
        if let Some(c) = child.as_mut() {
            if let Some(status) = c.try_wait()? {
                // Exit 2 means another starter won the race; keep polling for it.
                if status.code() != Some(EXIT_ALREADY_RUNNING) {
                    bail!(
                        "the dashbeam node failed to start ({status}); see {}{}",
                        log.display(),
                        log_tail(&log)
                    );
                }
                child = None;
            }
        }
        match Client::connect(data_dir).await {
            Ok(client) => {
                if let Some(mut c) = child.take() {
                    // Reap it when it exits so a long-lived caller (mcp) leaves no zombie.
                    std::thread::spawn(move || c.wait());
                }
                return Ok(client);
            }
            Err(e) if ipc::is_not_running(&e) => {
                // The lock holder may be a node shutting down (socket already
                // gone, lock still held for a few seconds): spawn once it lets go.
                if child.is_none() {
                    child = spawn_if_unlocked(data_dir)?;
                }
            }
            Err(e) => return Err(e),
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out after {}s waiting for the dashbeam node to start; see {}",
                START_TIMEOUT.as_secs(),
                log.display()
            );
        }
    }
}

/// Spawns a background node if no node holds the lock, first removing the
/// stale socket. `None` while the lock is held (a node starting or stopping).
fn spawn_if_unlocked(data_dir: &Path) -> Result<Option<Child>> {
    let lock = match NodeLock::acquire(data_dir) {
        Ok(lock) => lock,
        Err(e) if e.to_string().contains("already running") => return Ok(None),
        Err(e) => return Err(e),
    };
    let sock = agent::paths::socket_path(data_dir);
    match std::fs::remove_file(&sock) {
        Ok(()) => tracing::debug!("removed stale socket {}", sock.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", sock.display())),
    }
    drop(lock);
    spawn_background(data_dir).map(Some)
}

fn spawn_background(data_dir: &Path) -> Result<Child> {
    let exe = std::env::current_exe().context("locate the dashbeam binary")?;
    let log_path = agent::paths::log_path(data_dir);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg("up")
        .arg("--background")
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // Never pin the caller's directory; the daemon gets absolute paths only.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.current_dir("/");
        // Own process group: the caller's Ctrl-C or terminal hangup doesn't reach it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The client was spawned with an inheritable stdout pipe (`Command::output`).
        // Windows gives that handle to every child, so the daemon would keep the
        // pipe open and the caller's `output()` would wait forever. Drop the
        // inherit flag before spawning; Rust marks the daemon's own stdio itself.
        clear_stdio_inherit();
        // CREATE_NEW_PROCESS_GROUP: the caller's Ctrl-C doesn't reach the daemon.
        cmd.creation_flags(0x00000200);
        cmd.current_dir(data_dir);
    }
    cmd.spawn().context("spawn dashbeam up --background")
}

#[cfg(windows)]
fn clear_stdio_inherit() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    unsafe {
        for handle in [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            if !handle.is_null() {
                let _ = SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Last few log lines, for a start failure message.
fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().rev().take(5).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from(":");
    for l in lines.into_iter().rev() {
        out.push_str("\n  ");
        out.push_str(l);
    }
    out
}
