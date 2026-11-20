//! Opt-in Linux control API for desktop-shell integrations.
//!
//! One JSON request/response per private UNIX connection. No TCP listener or
//! shell execution. Only processes running as this user may connect.
use crate::{commands, state::AppStateMutex};
use engine::TransferHistoryStore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{Listener, Manager};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};

const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 8;
type Events = Arc<Mutex<BTreeMap<String, Value>>>;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Status {},
    Send {
        paths: Vec<String>,
        #[serde(default)]
        endpoint_id: Option<String>,
        #[serde(default)]
        nearby: bool,
    },
    Receive {
        ticket: String,
        output: String,
    },
    Stop {},
    Respond {
        endpoint_id: String,
        accepted: bool,
        #[serde(default)]
        output: Option<String>,
    },
    Pairing {},
    Join {
        ticket: String,
    },
    PairNearby {
        endpoint_id: String,
    },
}

fn parse_payload(raw: &str) -> Value {
    let outer = serde_json::from_str(raw).unwrap_or_else(|_| json!(raw));
    if let Value::String(s) = &outer {
        serde_json::from_str(s).unwrap_or(outer)
    } else {
        outer
    }
}

fn clear_transfer_events(events: &Events) {
    if let Ok(mut map) = events.lock() {
        map.retain(|k, _| !k.starts_with("transfer-") && !k.starts_with("receive-"));
    }
}

fn require_private_directory(path: &Path, uid: u32) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(meta.is_dir(), "control directory must not be a symlink");
    anyhow::ensure!(
        meta.uid() == uid,
        "control directory must belong to this user"
    );
    anyhow::ensure!(
        meta.mode() & 0o077 == 0,
        "control directory must be private (0700)"
    );
    Ok(())
}

fn prepare_socket(runtime: &Path, uid: u32) -> anyhow::Result<PathBuf> {
    require_private_directory(runtime, uid)?;
    let dir = runtime.join("dashbeam");
    match std::fs::create_dir(&dir) {
        Ok(()) => std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    require_private_directory(&dir, uid)?;
    let path = dir.join("control.sock");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.file_type().is_socket() && meta.uid() == uid,
                "refusing to replace non-socket or foreign control path"
            );
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(_) => anyhow::bail!("local control is already running"),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) => {}
                Err(e) => return Err(e.into()),
            }
            std::fs::remove_file(&path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(path)
}

// A clean shutdown removes only our own socket. A crash leaves a stale socket
// that prepare_socket can recover without overwriting files or live listeners.
struct SocketPath {
    path: PathBuf,
    inode: u64,
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.ino() == self.inode && m.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn read_request(reader: impl AsyncRead + Unpin) -> Result<Request, String> {
    let mut reader = BufReader::new(reader.take(MAX_REQUEST_BYTES + 1));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .map_err(|_| "Request timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    if line.len() as u64 > MAX_REQUEST_BYTES || !line.ends_with('\n') {
        return Err("Expected a newline-terminated JSON request of at most 64 KiB".into());
    }
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

pub fn start(app: tauri::AppHandle) -> anyhow::Result<()> {
    if std::env::var("DASHBEAM_LOCAL_CONTROL").as_deref() != Ok("1") {
        return Ok(());
    }
    let runtime = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is not set"))?,
    );
    // This module is Linux-only; /proc/self reflects the effective user even
    // when a launcher supplies an unexpected runtime directory.
    let uid = std::fs::metadata("/proc/self")?.uid();
    let path = prepare_socket(&runtime, uid)?;
    let std_listener = std::os::unix::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let socket_path = SocketPath {
        inode: std::fs::symlink_metadata(&path)?.ino(),
        path,
    };
    std_listener.set_nonblocking(true)?;
    let listener = {
        let handle = tauri::async_runtime::handle();
        let _guard = handle.inner().enter();
        UnixListener::from_std(std_listener)?
    };
    let events: Events = Arc::new(Mutex::new(BTreeMap::new()));
    for name in [
        "transfer-started",
        "transfer-progress",
        "transfer-completed",
        "transfer-failed",
        "receive-progress",
        "receive-completed",
        "receive-error",
        "paired-invite-received",
        "paired-invite-response",
        "device-paired",
    ] {
        let events = events.clone();
        let key = name.to_owned();
        app.listen(name, move |e| {
            if let Ok(mut map) = events.lock() {
                map.insert(key.clone(), parse_payload(e.payload()));
            }
        });
    }
    let receive_busy = Arc::new(AtomicBool::new(false));
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tauri::async_runtime::spawn(async move {
        let _socket_path = socket_path;
        while let Ok((stream, _)) = listener.accept().await {
            if stream.peer_cred().map(|c| c.uid()).ok() != Some(uid) {
                continue;
            }
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let app = app.clone();
            let events = events.clone();
            let receive_busy = receive_busy.clone();
            tauri::async_runtime::spawn(async move {
                let _permit = permit;
                serve(stream, app, events, receive_busy).await;
            });
        }
    });
    Ok(())
}

async fn serve(
    stream: UnixStream,
    app: tauri::AppHandle,
    events: Events,
    receive_busy: Arc<AtomicBool>,
) {
    let (read, mut write) = stream.into_split();
    let result = match read_request(read).await {
        Ok(req) => dispatch(&app, req, events, receive_busy).await,
        Err(e) => Err(e),
    };
    let response = match result {
        Ok(data) => json!({"ok": true, "data": data}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        write.write_all(format!("{response}\n").as_bytes()),
    )
    .await;
}

fn absolute_path(value: &str) -> Result<(), String> {
    if !Path::new(value).is_absolute() {
        return Err("File paths must be absolute".into());
    }
    Ok(())
}

// Avoid spawning an unbounded number of pending receives from repeated clicks.
struct ReceiveSlot(Arc<AtomicBool>);
impl ReceiveSlot {
    fn acquire(busy: Arc<AtomicBool>) -> Result<Self, String> {
        busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "A receive is already running".to_owned())?;
        Ok(Self(busy))
    }
}
impl Drop for ReceiveSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn spawn_receive(
    app: tauri::AppHandle,
    ticket: String,
    output: String,
    events: Events,
    slot: ReceiveSlot,
) {
    clear_transfer_events(&events);
    tauri::async_runtime::spawn(async move {
        let _slot = slot;
        let result = commands::receive_file(
            ticket,
            output,
            None,
            None,
            None,
            None,
            None,
            app.state::<AppStateMutex>(),
            app.state::<Arc<TransferHistoryStore>>(),
            app.clone(),
        )
        .await;
        if let Ok(mut map) = events.lock() {
            match result {
                Ok(path) => {
                    map.insert("receive-completed".into(), json!(path));
                }
                Err(e) => {
                    map.insert("receive-error".into(), json!(e));
                }
            }
        }
    });
}

async fn dispatch(
    app: &tauri::AppHandle,
    req: Request,
    events: Events,
    receive_busy: Arc<AtomicBool>,
) -> Result<Value, String> {
    let state = || app.state::<AppStateMutex>();
    let history = || app.state::<Arc<TransferHistoryStore>>();
    match req {
        Request::Status {} => {
            let node = commands::get_node_status(state()).await?;
            let paired = commands::list_paired_devices(state())
                .await
                .unwrap_or_default();
            let nearby = commands::list_nearby(state()).await.unwrap_or_default();
            let ticket = commands::get_sharing_status(state()).await?;
            let active = state().lock().await.is_transporting;
            let snapshot = events.lock().map_err(|e| e.to_string())?.clone();
            Ok(
                json!({"api_version": 1, "node": node, "paired": paired, "nearby": nearby, "ticket": ticket, "active": active, "events": snapshot}),
            )
        }
        Request::Send {
            paths,
            endpoint_id,
            nearby,
        } => {
            if paths.is_empty() {
                return Err("No paths provided".into());
            }
            for path in &paths {
                absolute_path(path)?;
            }
            let count = u32::try_from(paths.len()).map_err(|e| e.to_string())?;
            clear_transfer_events(&events);
            let ticket =
                commands::send_items(paths, None, None, state(), history(), app.clone()).await?;
            // The share already computed its size; don't rescan folders on a
            // Tokio worker or count nested files as top-level selected items.
            let bytes = state()
                .lock()
                .await
                .current_share
                .as_ref()
                .map(|s| s.send_result.size)
                .unwrap_or(0);
            if let Some(target) = endpoint_id.filter(|s| !s.is_empty()) {
                let result = if nearby {
                    commands::invite_nearby_device(target, ticket.clone(), count, bytes, state())
                        .await
                } else {
                    commands::invite_paired_device(target, ticket.clone(), count, bytes, state())
                        .await
                };
                // Keep the ticket usable even when the destination is offline.
                return Ok(match result {
                    Ok(delivered) => json!({"ticket": ticket, "delivered": delivered.delivered}),
                    Err(error) => {
                        json!({"ticket": ticket, "delivered": false, "invite_error": error})
                    }
                });
            }
            Ok(json!({"ticket": ticket}))
        }
        Request::Receive { ticket, output } => {
            absolute_path(&output)?;
            let slot = ReceiveSlot::acquire(receive_busy)?;
            spawn_receive(app.clone(), ticket, output, events, slot);
            Ok(json!({"started": true}))
        }
        Request::Stop {} => {
            commands::cancel_receive(state()).await?;
            commands::stop_sharing(state()).await?;
            Ok(json!(true))
        }
        Request::Respond {
            endpoint_id,
            accepted,
            output,
        } => {
            // Consume a real pending invitation; clients cannot substitute a
            // ticket while acknowledging a different sender's invitation.
            let invite = events
                .lock()
                .map_err(|e| e.to_string())?
                .get("paired-invite-received")
                .cloned()
                .ok_or("No pending invitation")?;
            if invite["remote_endpoint_id"].as_str() != Some(endpoint_id.as_str()) {
                return Err("Invitation sender does not match".into());
            }
            let receive = if accepted {
                let output = output.ok_or("Missing output directory")?;
                absolute_path(&output)?;
                let ticket = invite["blob_ticket"]
                    .as_str()
                    .ok_or("Invitation has no ticket")?
                    .to_owned();
                Some((ticket, output, ReceiveSlot::acquire(receive_busy)?))
            } else {
                None
            };
            let paired = commands::list_paired_devices(state())
                .await?
                .iter()
                .any(|d| d.endpoint_id == endpoint_id && d.pairing_status.is_active());
            if paired {
                commands::respond_paired_invite(endpoint_id, accepted, state()).await?;
            } else {
                commands::respond_nearby_invite(endpoint_id, accepted, false, state()).await?;
            }
            if let Ok(mut map) = events.lock() {
                // Do not discard a newer invitation that arrived while awaiting
                // the response handshake.
                if map.get("paired-invite-received") == Some(&invite) {
                    map.remove("paired-invite-received");
                }
            }
            if let Some((ticket, output, slot)) = receive {
                spawn_receive(app.clone(), ticket, output, events, slot);
            }
            Ok(json!(true))
        }
        Request::Pairing {} => Ok(json!(
            commands::start_pairing_host(Some(120), state()).await?
        )),
        Request::Join { ticket } => {
            commands::join_pairing(ticket, state()).await?;
            Ok(json!(true))
        }
        Request::PairNearby { endpoint_id } => {
            let result = commands::request_nearby_pair(endpoint_id, state()).await?;
            Ok(json!({"delivered": result.delivered}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn uid() -> u32 {
        std::fs::metadata("/proc/self").unwrap().uid()
    }

    fn private_runtime() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[tokio::test]
    async fn framing_rejects_unterminated_oversized_and_unknown_requests() {
        assert!(read_request(&b"{\"action\":\"status\"}"[..]).await.is_err());
        let oversized = format!("{}\n", " ".repeat(MAX_REQUEST_BYTES as usize));
        assert!(read_request(oversized.as_bytes()).await.is_err());
        assert!(
            read_request(&b"{\"action\":\"exec\",\"command\":\"anything\"}\n"[..])
                .await
                .is_err()
        );
        assert!(
            read_request(&b"{\"action\":\"status\",\"unexpected\":true}\n"[..])
                .await
                .is_err()
        );
        assert!(matches!(
            read_request(&b"{\"action\":\"status\"}\n"[..])
                .await
                .unwrap(),
            Request::Status {}
        ));
    }

    #[test]
    fn stale_socket_is_recovered_but_live_socket_is_preserved() {
        let runtime = private_runtime();
        let path = prepare_socket(runtime.path(), uid()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(prepare_socket(runtime.path(), uid()).is_err());
        assert!(path.exists());
        drop(listener);
        assert_eq!(prepare_socket(runtime.path(), uid()).unwrap(), path);
        assert!(!path.exists());
    }

    #[test]
    fn refuses_symlink_and_non_socket_paths_without_changing_them() {
        let runtime = private_runtime();
        let path = prepare_socket(runtime.path(), uid()).unwrap();
        let target = runtime.path().join("user-file");
        std::fs::write(&target, "keep me").unwrap();
        symlink(&target, &path).unwrap();
        assert!(prepare_socket(runtime.path(), uid()).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "also keep me").unwrap();
        assert!(prepare_socket(runtime.path(), uid()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "also keep me");
    }

    #[test]
    fn refuses_public_or_foreign_runtime_directories() {
        let runtime = private_runtime();
        assert!(prepare_socket(runtime.path(), uid().wrapping_add(1)).is_err());
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_socket(runtime.path(), uid()).is_err());
    }

    #[test]
    fn receive_slot_is_released_after_error_or_completion() {
        let busy = Arc::new(AtomicBool::new(false));
        let slot = ReceiveSlot::acquire(busy.clone()).unwrap();
        assert!(ReceiveSlot::acquire(busy.clone()).is_err());
        drop(slot);
        assert!(ReceiveSlot::acquire(busy).is_ok());
    }
}
