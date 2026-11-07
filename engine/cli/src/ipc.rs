//! The node socket: newline-delimited JSON over a local socket in the data
//! dir, reachable by the same OS user. macOS and Linux use a Unix socket
//! (`0600`). Windows uses a randomly named pipe, open only to the current user
//! and to local clients; `node.sock` is a marker file holding its name.
//!
//! 1. Client sends `{"hello":{"version":1}}`; the server answers
//!    `{"ok":NodeInfo}` or, on a version mismatch, `{"error":"…"}` and closes.
//!    `{"hello":{"version":N,"stop":true}}` stops the node whatever `N` is, so
//!    `dashbeam stop` works across versions; keep that in every future version.
//! 2. Then one [`Request`] line per call, each answered by `{"ok":<value>}`
//!    or `{"error":"<message>"}`. Requests on one connection run in order.
//!
//! Paths in requests are absolute or relative to the request's `cwd`, which
//! must be absolute (the caller's working directory, never the daemon's).

pub mod transport;

use agent::registry::{Direction, TransferRecord};
use agent::AgentService;
use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::sync::watch;
use tokio::task::JoinSet;

pub const PROTOCOL_VERSION: u32 = 1;
/// Upper bound for `wait_*` requests.
pub const MAX_WAIT_SECS: u64 = 120;
/// How long a client waits for the hello reply.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartedBy {
    /// `dashbeam up` in a terminal.
    Hand,
    /// Spawned in the background by a client (`up --background`).
    Auto,
}

/// The hello reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub version: u32,
    pub node: engine::DeviceInfo,
    pub agent_pairing_allowed: bool,
    pub started_by: StartedBy,
    pub pid: u32,
    pub data_dir: PathBuf,
}

/// Reply to [`Request::Status`]: the hello info (with a fresh `node`) plus counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    #[serde(flatten)]
    pub info: NodeInfo,
    pub uptime_secs: u64,
    pub receive_dir: PathBuf,
    pub paired_devices: usize,
    pub online_devices: usize,
    pub active_transfers: usize,
    pub pending_invites: usize,
    #[serde(default)]
    pub unread_messages: usize,
}

/// Reply to [`Request::WaitTransfer`]: the record plus whether it timed out
/// before reaching a terminal status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitResult {
    #[serde(flatten)]
    pub record: TransferRecord,
    pub still_running: bool,
}

/// Reply to [`Request::WaitIncoming`]: received transfers with `seq > after_seq`
/// and the cursor to pass next time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingBatch {
    pub transfers: Vec<TransferRecord>,
    pub next_seq: u64,
}

/// One per `AgentService` operation. The `agent_*` variants are the gated
/// ones for MCP; the plain pairing ops are for the human CLI only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// -> `DeviceInfo`
    Identity,
    /// -> [`StatusInfo`]
    Status,
    /// -> `[PairedDeviceInfo]`
    ListDevices,
    /// -> `[NearbyDevice]`
    ListNearby,
    /// -> `DeviceInfo`
    Rename { name: String },
    /// -> `TransferRecord`
    SendToDevice {
        device: String,
        paths: Vec<PathBuf>,
        cwd: PathBuf,
    },
    /// -> `TransferRecord` (with `ticket` and `expires_at`)
    ShareTicket {
        paths: Vec<PathBuf>,
        cwd: PathBuf,
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    /// -> `TransferRecord`, still `transferring`; wait on it for the outcome.
    ReceiveTicket { ticket: String },
    /// -> `TransferRecord`
    GetTransfer { id: String },
    /// -> [`WaitResult`]; `timeout_secs` is capped at [`MAX_WAIT_SECS`].
    WaitTransfer { id: String, timeout_secs: u64 },
    /// -> `TransferRecord`
    CancelTransfer { id: String },
    /// -> `[TransferRecord]`
    ListTransfers {
        #[serde(default)]
        after_seq: Option<u64>,
        #[serde(default)]
        direction: Option<Direction>,
    },
    /// -> [`IncomingBatch`]; `timeout_secs` is capped at [`MAX_WAIT_SECS`].
    WaitIncoming { after_seq: u64, timeout_secs: u64 },
    /// -> `[PendingInvite]`
    ListIncoming,
    /// -> `TransferRecord`
    AcceptIncoming { invite_id: String },
    /// -> `null`
    DeclineIncoming { invite_id: String },
    /// -> pairing code string
    PairingCode,
    /// -> pairing code string
    HostPairing {
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    /// -> `null`
    Join { code: String },
    /// -> `bool` (whether the request was delivered)
    PairNearby { device: String },
    /// -> `PairedDeviceInfo`
    Trust { device: String, trusted: bool },
    /// -> `null`
    Forget { device: String },
    /// Gated by `--dangerously-allow-agent-pairing`. -> `bool`
    AgentPairNearby { device: String },
    /// Gated by `--dangerously-allow-agent-pairing`. -> `PairedDeviceInfo`
    AgentTrust { device: String, trusted: bool },
    /// -> `Message`; fails when the device is unreachable.
    SendMessage { device: String, text: String },
    /// -> `[Message]`, both directions, oldest first. Doesn't mark anything read.
    ListMessages {
        #[serde(default)]
        after_seq: Option<u64>,
    },
    /// -> `[Message]`: unread inbound messages, now marked read; waits up to
    /// `timeout_secs` (capped at [`MAX_WAIT_SECS`]) for at least one.
    WaitMessages { timeout_secs: u64 },
    /// -> `null`, then the node shuts down.
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Ok(Value),
    Error(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct HelloLine {
    hello: Hello,
}

#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    version: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    stop: bool,
}

/// No node is serving this data dir (socket missing or refusing).
#[derive(Debug)]
pub struct NotRunning {
    pub data_dir: PathBuf,
    pub source: std::io::Error,
}

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no dashbeam node is running for {}",
            self.data_dir.display()
        )
    }
}

impl std::error::Error for NotRunning {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Fails if `data_dir`'s socket path can't be bound (e.g. too long).
pub fn check_socket_path(data_dir: &Path) -> Result<()> {
    transport::check_path(&agent::paths::socket_path(data_dir))
}

/// True when `err` came from [`Client::connect`] finding no node.
pub fn is_not_running(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotRunning>().is_some()
}

fn reply_line(reply: &Reply) -> String {
    let mut s = serde_json::to_string(reply).unwrap_or_else(|e| {
        serde_json::to_string(&Reply::Error(format!("encode reply: {e}"))).expect("string")
    });
    s.push('\n');
    s
}

// ---------------------------------------------------------------- server

struct Ctx {
    svc: Arc<AgentService>,
    started_by: StartedBy,
    started_at: Instant,
    stop_tx: watch::Sender<bool>,
}

impl Ctx {
    fn info(&self) -> NodeInfo {
        NodeInfo {
            version: PROTOCOL_VERSION,
            node: self.svc.identity(),
            agent_pairing_allowed: self.svc.allow_agent_pairing(),
            started_by: self.started_by,
            pid: std::process::id(),
            data_dir: self.svc.data_dir().to_path_buf(),
        }
    }

    fn status(&self) -> Result<StatusInfo> {
        let devices = self.svc.list_devices()?;
        let active = |d: &&engine::PairedDeviceInfo| d.pairing_status.is_active();
        Ok(StatusInfo {
            info: self.info(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            receive_dir: self.svc.config().receive_dir(self.svc.data_dir()),
            paired_devices: devices.iter().filter(active).count(),
            online_devices: devices.iter().filter(active).filter(|d| d.online).count(),
            active_transfers: self
                .svc
                .registry()
                .list(None, None)
                .iter()
                .filter(|r| !r.status.is_terminal())
                .count(),
            pending_invites: self.svc.list_incoming().len(),
            unread_messages: self.svc.messages().unread_count(),
        })
    }

    fn request_stop(&self) {
        let _ = self.stop_tx.send(true);
    }
}

/// Serves one `AgentService` on its data dir's socket.
pub struct Server {
    listener: transport::Listener,
    path: PathBuf,
    ctx: Arc<Ctx>,
}

impl Server {
    /// Binds `socket_path(svc.data_dir())`, replacing any stale socket. The
    /// caller must hold the data dir's `NodeLock`.
    pub fn bind(svc: Arc<AgentService>, started_by: StartedBy) -> Result<Self> {
        let path = agent::paths::socket_path(svc.data_dir());
        let listener = transport::bind(&path)?;
        let (stop_tx, _) = watch::channel(false);
        Ok(Self {
            listener,
            path,
            ctx: Arc::new(Ctx {
                svc,
                started_by,
                started_at: Instant::now(),
                stop_tx,
            }),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Serves until `shutdown` resolves or a client asks the node to stop,
    /// then removes the socket. Does not shut the `AgentService` down.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let mut stop_rx = self.ctx.stop_tx.subscribe();
        let mut conns = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = async { let _ = stop_rx.wait_for(|stop| *stop).await; } => break,
                res = self.listener.accept() => match res {
                    Ok(stream) => {
                        conns.spawn(handle_conn(self.ctx.clone(), stream));
                    }
                    Err(e) => {
                        tracing::warn!("socket accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
        // New clients see "not running" from here on; replies already written
        // (e.g. to Shutdown) were flushed before the stop flag was set.
        drop(self.listener);
        let _ = std::fs::remove_file(&self.path);
        conns.shutdown().await;
        Ok(())
    }
}

async fn write_reply(w: &mut transport::WriteHalf, reply: &Reply) -> std::io::Result<()> {
    w.write_all(reply_line(reply).as_bytes()).await?;
    w.flush().await
}

async fn handle_conn(ctx: Arc<Ctx>, stream: transport::Stream) {
    let (r, mut w) = transport::split(stream);
    let mut lines = BufReader::new(r).lines();
    let Ok(Some(first)) = lines.next_line().await else {
        return;
    };
    let hello = match serde_json::from_str::<HelloLine>(&first) {
        Ok(h) => h.hello,
        Err(_) => {
            let msg = "expected {\"hello\":{\"version\":N}} as the first line".to_string();
            let _ = write_reply(&mut w, &Reply::Error(msg)).await;
            return;
        }
    };
    if hello.stop {
        let info = serde_json::to_value(ctx.info()).unwrap_or(Value::Null);
        let _ = write_reply(&mut w, &Reply::Ok(info)).await;
        tracing::info!("stop requested over the socket");
        ctx.request_stop();
        return;
    }
    if hello.version != PROTOCOL_VERSION {
        let msg = format!(
            "a dashbeam node from another version is running (protocol {PROTOCOL_VERSION}); run `dashbeam stop`"
        );
        let _ = write_reply(&mut w, &Reply::Error(msg)).await;
        return;
    }
    let info = serde_json::to_value(ctx.info()).unwrap_or(Value::Null);
    if write_reply(&mut w, &Reply::Ok(info)).await.is_err() {
        return;
    }

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req = match serde_json::from_str::<Request>(&line) {
            Ok(r) => r,
            Err(e) => {
                let reply = Reply::Error(format!("bad request: {e}"));
                if write_reply(&mut w, &reply).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let shutdown = matches!(req, Request::Shutdown);
        let reply = match dispatch(&ctx, req).await {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Error(format!("{e:#}")),
        };
        let written = write_reply(&mut w, &reply).await;
        if shutdown {
            tracing::info!("shutdown requested over the socket");
            ctx.request_stop();
            return;
        }
        if written.is_err() {
            return;
        }
    }
}

fn require_absolute(cwd: &Path) -> Result<()> {
    if !cwd.is_absolute() {
        bail!(
            "cwd must be an absolute path (the caller's working directory), got {}",
            cwd.display()
        );
    }
    Ok(())
}

fn capped(secs: u64) -> Duration {
    Duration::from_secs(secs.min(MAX_WAIT_SECS))
}

async fn dispatch(ctx: &Ctx, req: Request) -> Result<Value> {
    let svc = &ctx.svc;
    let unknown = |id: &str| anyhow!("no transfer with id {id}");
    let v = match req {
        Request::Identity => serde_json::to_value(svc.identity())?,
        Request::Status => serde_json::to_value(ctx.status()?)?,
        Request::ListDevices => serde_json::to_value(svc.list_devices()?)?,
        Request::ListNearby => serde_json::to_value(svc.list_nearby().await)?,
        Request::Rename { name } => serde_json::to_value(svc.rename(&name)?)?,
        Request::SendToDevice { device, paths, cwd } => {
            require_absolute(&cwd)?;
            serde_json::to_value(svc.send_to_device(&device, paths, cwd).await?)?
        }
        Request::ShareTicket {
            paths,
            cwd,
            ttl_secs,
        } => {
            require_absolute(&cwd)?;
            serde_json::to_value(svc.share_ticket(paths, cwd, ttl_secs).await?)?
        }
        Request::ReceiveTicket { ticket } => {
            serde_json::to_value(svc.receive_ticket(&ticket).await?)?
        }
        Request::GetTransfer { id } => {
            serde_json::to_value(svc.registry().get(&id).ok_or_else(|| unknown(&id))?)?
        }
        Request::WaitTransfer { id, timeout_secs } => {
            let (record, still_running) = svc
                .registry()
                .wait_terminal(&id, capped(timeout_secs))
                .await
                .ok_or_else(|| unknown(&id))?;
            serde_json::to_value(WaitResult {
                record,
                still_running,
            })?
        }
        Request::CancelTransfer { id } => serde_json::to_value(svc.cancel_transfer(&id).await?)?,
        Request::ListTransfers {
            after_seq,
            direction,
        } => serde_json::to_value(svc.registry().list(after_seq, direction))?,
        Request::WaitIncoming {
            after_seq,
            timeout_secs,
        } => {
            let (transfers, next_seq) = svc
                .registry()
                .wait_inbound_completed(after_seq, capped(timeout_secs))
                .await;
            serde_json::to_value(IncomingBatch {
                transfers,
                next_seq,
            })?
        }
        Request::ListIncoming => serde_json::to_value(svc.list_incoming())?,
        Request::AcceptIncoming { invite_id } => {
            serde_json::to_value(svc.accept_incoming(&invite_id).await?)?
        }
        Request::DeclineIncoming { invite_id } => {
            svc.decline_incoming(&invite_id).await?;
            Value::Null
        }
        Request::PairingCode => Value::String(svc.pairing_code()?),
        Request::HostPairing { ttl_secs } => Value::String(svc.host_pairing(ttl_secs).await?),
        Request::Join { code } => {
            svc.join(&code).await?;
            Value::Null
        }
        Request::PairNearby { device } => Value::Bool(svc.pair_nearby(&device).await?),
        Request::Trust { device, trusted } => serde_json::to_value(svc.trust(&device, trusted)?)?,
        Request::Forget { device } => {
            svc.forget(&device).await?;
            Value::Null
        }
        Request::AgentPairNearby { device } => Value::Bool(svc.agent_pair_nearby(&device).await?),
        Request::AgentTrust { device, trusted } => {
            serde_json::to_value(svc.agent_trust(&device, trusted).await?)?
        }
        Request::SendMessage { device, text } => {
            serde_json::to_value(svc.send_message(&device, &text).await?)?
        }
        Request::ListMessages { after_seq } => {
            serde_json::to_value(svc.messages().list(after_seq))?
        }
        Request::WaitMessages { timeout_secs } => {
            serde_json::to_value(svc.messages().wait_unread(capped(timeout_secs)).await)?
        }
        Request::Shutdown => Value::Null,
    };
    Ok(v)
}

// ---------------------------------------------------------------- client

/// A connection to the node serving a data dir.
pub struct Client {
    lines: Lines<BufReader<transport::ReadHalf>>,
    writer: transport::WriteHalf,
    hello: NodeInfo,
}

/// Connects and sends the hello; returns the raw hello reply value.
async fn open(
    data_dir: &Path,
    stop: bool,
) -> Result<(
    Lines<BufReader<transport::ReadHalf>>,
    transport::WriteHalf,
    Value,
)> {
    let path = agent::paths::socket_path(data_dir);
    let stream = transport::connect(&path).await.map_err(|e| {
        if transport::is_not_running(&e) {
            anyhow::Error::new(NotRunning {
                data_dir: data_dir.to_path_buf(),
                source: e,
            })
        } else if e.kind() == std::io::ErrorKind::InvalidInput {
            anyhow!(e.to_string())
        } else {
            anyhow::Error::new(e).context(format!("connect to {}", path.display()))
        }
    })?;
    let (r, mut writer) = transport::split(stream);
    let mut lines = BufReader::new(r).lines();
    let mut hello = serde_json::to_string(&HelloLine {
        hello: Hello {
            version: PROTOCOL_VERSION,
            stop,
        },
    })?;
    hello.push('\n');
    writer.write_all(hello.as_bytes()).await?;
    writer.flush().await?;
    let line = tokio::time::timeout(HELLO_TIMEOUT, lines.next_line())
        .await
        .map_err(|_| anyhow!("the dashbeam node at {} did not answer", path.display()))??
        .ok_or_else(|| anyhow!("the dashbeam node closed the connection during hello"))?;
    let info: Value = decode_reply(&line)?;
    Ok((lines, writer, info))
}

fn decode_reply<T: DeserializeOwned>(line: &str) -> Result<T> {
    match serde_json::from_str::<Reply>(line).context("malformed reply from the dashbeam node")? {
        Reply::Ok(v) => {
            serde_json::from_value(v).context("unexpected reply from the dashbeam node")
        }
        Reply::Error(e) => Err(anyhow!(e)),
    }
}

impl Client {
    /// Connects and says hello. Fails with [`NotRunning`] (see
    /// [`is_not_running`]) when no node serves `data_dir`.
    pub async fn connect(data_dir: &Path) -> Result<Client> {
        let (lines, writer, info) = open(data_dir, false).await?;
        let hello =
            serde_json::from_value(info).context("unexpected hello from the dashbeam node")?;
        Ok(Client {
            lines,
            writer,
            hello,
        })
    }

    /// The node's hello reply.
    pub fn hello(&self) -> &NodeInfo {
        &self.hello
    }

    pub async fn call<T: DeserializeOwned>(&mut self, req: Request) -> Result<T> {
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        let reply = self
            .lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("the dashbeam node closed the connection"))?;
        decode_reply(&reply)
    }
}

/// A stopping node's pid, read loosely so any protocol version's hello works.
#[derive(Debug, Clone, Copy)]
pub struct Stopping {
    pub pid: Option<u32>,
}

/// Asks the node serving `data_dir` to stop, whatever its protocol version.
/// `None` when no node is running.
pub async fn request_stop(data_dir: &Path) -> Result<Option<Stopping>> {
    match open(data_dir, true).await {
        Ok((_, _, info)) => Ok(Some(Stopping {
            pid: info["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
        })),
        Err(e) if is_not_running(&e) => Ok(None),
        Err(e) => Err(e),
    }
}
