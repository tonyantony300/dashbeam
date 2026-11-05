//! `dashbeam mcp`: a stdio MCP server that forwards each tool call to the
//! node over its socket.
//!
//! This tool list is the security boundary (see the design's trust model):
//! pairing and trust are reachable only through the gated `agent_*` node ops,
//! and only when the node runs with `--dangerously-allow-agent-pairing`.
//! Without the flag those tools are not registered at all.

use crate::commands::absolutize;
use crate::daemon;
use crate::ipc::{self, Client, IncomingBatch, Request};
use agent::registry::{Direction, TransferRecord};
use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// `timeout_secs` when a wait tool gets none.
const DEFAULT_WAIT_SECS: u64 = 60;

/// The wait tools' timeout: the request (or a default), capped at
/// [`ipc::MAX_WAIT_SECS`].
pub(crate) fn clamp_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_WAIT_SECS)
        .min(ipc::MAX_WAIT_SECS)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendParams {
    /// Paired device name or endpoint id (from list_devices).
    pub device: String,
    /// Files or folders: absolute, or relative to this server's working directory.
    pub paths: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ShareParams {
    /// Files or folders: absolute, or relative to this server's working directory.
    pub paths: Vec<String>,
    /// Ticket lifetime in seconds (default: the node's ticket_ttl_secs, 1 hour).
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TicketParams {
    /// A DashBeam or sendme ticket.
    pub ticket: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TransferParams {
    /// The transfer_id from a transfer record.
    pub transfer_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitTransferParams {
    /// The transfer_id from a transfer record.
    pub transfer_id: String,
    /// Seconds to wait (default 60, max 120).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitIncomingParams {
    /// Seconds to wait (default 60, max 120).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectionParam {
    Inbound,
    Outbound,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListTransfersParams {
    /// Only records whose `seq` is greater than this (a `seq` from an earlier result).
    #[serde(default)]
    pub since: Option<u64>,
    /// `inbound` or `outbound`; both when omitted.
    #[serde(default)]
    pub direction: Option<DirectionParam>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InviteParams {
    /// The invite_id from list_incoming.
    pub invite_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeviceParams {
    /// Device name or endpoint id.
    pub device: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TrustParams {
    /// Paired device name or endpoint id (from list_devices).
    pub device: String,
    /// true to trust (default), false to stop trusting.
    #[serde(default)]
    pub trusted: Option<bool>,
}

/// `wait_for_incoming`'s position in the node's registry. A node restart
/// (new pid) starts a fresh registry, so the cursor resets.
#[derive(Debug)]
struct Cursor {
    pid: u32,
    seq: u64,
}

#[derive(Clone)]
pub struct DashbeamMcp {
    data_dir: PathBuf,
    pairing_allowed: bool,
    cursor: Arc<Mutex<Cursor>>,
    tool_router: ToolRouter<Self>,
}

fn ok(v: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn err(e: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))])
}

fn respond(r: Result<Value>) -> CallToolResult {
    r.map_or_else(err, ok)
}

impl DashbeamMcp {
    /// Connects to (or auto-starts) the node for `data_dir`. The pairing
    /// tools are registered only if that node allows agent pairing.
    pub async fn connect(data_dir: &Path) -> Result<Self> {
        let mut c = daemon::ensure_node(data_dir).await?;
        let pairing_allowed = c.hello().agent_pairing_allowed;
        let pid = c.hello().pid;
        // Report only what is received from now on, not the node's history.
        let all: Vec<TransferRecord> = c
            .call(Request::ListTransfers {
                after_seq: None,
                direction: None,
            })
            .await?;
        let seq = all.iter().map(|r| r.seq).max().unwrap_or(0);
        let mut tool_router = Self::base_tools();
        if pairing_allowed {
            tool_router.merge(Self::pairing_tools());
        }
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            pairing_allowed,
            cursor: Arc::new(Mutex::new(Cursor { pid, seq })),
            tool_router,
        })
    }

    /// One connection per tool call: a connection serves one request at a
    /// time and waits can block for minutes. Restarts the node if it's gone.
    async fn node(&self) -> Result<Client> {
        daemon::ensure_node(&self.data_dir).await
    }

    async fn call(&self, req: Request) -> Result<Value> {
        self.node().await?.call(req).await
    }

    fn paths(paths: &[String]) -> Result<(Vec<PathBuf>, PathBuf)> {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        if paths.is_empty() {
            anyhow::bail!("paths must not be empty");
        }
        absolutize(&paths)
    }
}

#[tool_router(router = base_tools)]
impl DashbeamMcp {
    #[tool(
        description = "This DashBeam agent node's own identity: display_name, endpoint_id, device_type (\"agent\"), and agent_pairing_allowed. online is true while the node is running."
    )]
    async fn get_identity(&self) -> CallToolResult {
        respond(
            async {
                let mut c = self.node().await?;
                let allowed = c.hello().agent_pairing_allowed;
                let mut me: Value = c.call(Request::Identity).await?;
                if let Value::Object(m) = &mut me {
                    m.insert("online".into(), Value::Bool(true));
                    m.insert("agent_pairing_allowed".into(), Value::Bool(allowed));
                }
                Ok(me)
            }
            .await,
        )
    }

    #[tool(
        description = "Your paired devices, the user's trusted network: display_name, endpoint_id, device_type, online, trusted. Only these can be sent to with send_to_device."
    )]
    async fn list_devices(&self) -> CallToolResult {
        respond(self.call(Request::ListDevices).await)
    }

    #[tool(
        description = "Devices seen on the local network, read-only, for telling the user what is around. Nearby devices are not paired and cannot be sent to."
    )]
    async fn list_nearby(&self) -> CallToolResult {
        respond(self.call(Request::ListNearby).await)
    }

    #[tool(
        description = "Sends to one of your paired devices. Trusted devices receive automatically; others must accept. Returns immediately; use wait_for_transfer for the outcome. 'expired' can also mean a busy receiver did not get to it in time. Paths are absolute or relative to this server's working directory. Returns the transfer record with its transfer_id."
    )]
    async fn send_to_device(&self, Parameters(p): Parameters<SendParams>) -> CallToolResult {
        respond(
            async {
                let (paths, cwd) = Self::paths(&p.paths)?;
                self.call(Request::SendToDevice {
                    device: p.device,
                    paths,
                    cwd,
                })
                .await
            }
            .await,
        )
    }

    #[tool(
        description = "Shares files behind a one-time ticket that can be handed to anyone, including people and agents outside the paired network. Anyone holding this ticket can download the files once, until it expires. Paths are absolute or relative to this server's working directory. Returns the transfer record with transfer_id, ticket and expires_at (RFC 3339 UTC; expires_at_ms is the same instant in unix ms); use wait_for_transfer to learn when it was downloaded."
    )]
    async fn share_ticket(&self, Parameters(p): Parameters<ShareParams>) -> CallToolResult {
        respond(
            async {
                let (paths, cwd) = Self::paths(&p.paths)?;
                self.call(Request::ShareTicket {
                    paths,
                    cwd,
                    ttl_secs: p.ttl_secs,
                })
                .await
            }
            .await,
        )
    }

    #[tool(
        description = "Starts downloading a DashBeam or sendme ticket into the node's receive folder and returns at once with the transfer record (status transferring) and its transfer_id; use wait_for_transfer for the outcome, whose saved_paths lists where the files landed. Files are never opened or executed and never overwrite existing files. If the node sets max_ticket_receive_bytes, a ticket over the limit, or a plain sendme ticket that carries no size, is rejected before downloading."
    )]
    async fn receive_ticket(&self, Parameters(p): Parameters<TicketParams>) -> CallToolResult {
        respond(self.call(Request::ReceiveTicket { ticket: p.ticket }).await)
    }

    #[tool(description = "The full record of one transfer.")]
    async fn transfer_status(&self, Parameters(p): Parameters<TransferParams>) -> CallToolResult {
        respond(self.call(Request::GetTransfer { id: p.transfer_id }).await)
    }

    #[tool(
        description = "Waits until a transfer reaches a final status (delivered, received, declined, expired, unreachable, failed, cancelled) or timeout_secs passes (default 60, max 120). still_running: true means it timed out; call again to keep waiting."
    )]
    async fn wait_for_transfer(
        &self,
        Parameters(p): Parameters<WaitTransferParams>,
    ) -> CallToolResult {
        respond(
            self.call(Request::WaitTransfer {
                id: p.transfer_id,
                timeout_secs: clamp_timeout(p.timeout_secs),
            })
            .await,
        )
    }

    #[tool(description = "Cancels a queued or running transfer. Returns its record.")]
    async fn cancel_transfer(&self, Parameters(p): Parameters<TransferParams>) -> CallToolResult {
        respond(
            self.call(Request::CancelTransfer { id: p.transfer_id })
                .await,
        )
    }

    #[tool(description = "Recent transfer records, oldest first.")]
    async fn list_transfers(
        &self,
        Parameters(p): Parameters<ListTransfersParams>,
    ) -> CallToolResult {
        let direction = p.direction.map(|d| match d {
            DirectionParam::Inbound => Direction::Inbound,
            DirectionParam::Outbound => Direction::Outbound,
        });
        respond(
            self.call(Request::ListTransfers {
                after_seq: p.since,
                direction,
            })
            .await,
        )
    }

    #[tool(
        description = "Waits up to timeout_secs (default 60, max 120) for files received from paired devices or tickets, and returns every inbound transfer completed since the previous call in this session, with saved_paths. Each transfer is reported once. An empty list means nothing arrived in time. Received files are never opened or executed."
    )]
    async fn wait_for_incoming(
        &self,
        Parameters(p): Parameters<WaitIncomingParams>,
    ) -> CallToolResult {
        respond(
            async {
                // Held across the wait so concurrent calls never report a transfer twice.
                let mut cursor = self.cursor.lock().await;
                let mut c = self.node().await?;
                if c.hello().pid != cursor.pid {
                    *cursor = Cursor {
                        pid: c.hello().pid,
                        seq: 0,
                    };
                }
                let batch: IncomingBatch = c
                    .call(Request::WaitIncoming {
                        after_seq: cursor.seq,
                        timeout_secs: clamp_timeout(p.timeout_secs),
                    })
                    .await?;
                cursor.seq = cursor.seq.max(batch.next_seq);
                Ok(json!({ "transfers": batch.transfers }))
            }
            .await,
        )
    }

    #[tool(
        description = "Pending invites from paired devices that are not trusted. Each waits for accept_incoming or decline_incoming until it times out. Files from trusted devices arrive without an invite. When the node runs with --dangerously-allow-agent-pairing this also lists invites from unpaired nearby devices; accepting one of those pairs that device."
    )]
    async fn list_incoming(&self) -> CallToolResult {
        respond(self.call(Request::ListIncoming).await)
    }

    #[tool(
        description = "Accepts a pending invite; the files download into the node's receive folder (never opened or executed). Returns the transfer record."
    )]
    async fn accept_incoming(&self, Parameters(p): Parameters<InviteParams>) -> CallToolResult {
        respond(
            self.call(Request::AcceptIncoming {
                invite_id: p.invite_id,
            })
            .await,
        )
    }

    #[tool(description = "Declines a pending invite; the sender sees 'declined'.")]
    async fn decline_incoming(&self, Parameters(p): Parameters<InviteParams>) -> CallToolResult {
        respond(
            async {
                self.call(Request::DeclineIncoming {
                    invite_id: p.invite_id.clone(),
                })
                .await?;
                Ok(json!({ "invite_id": p.invite_id, "declined": true }))
            }
            .await,
        )
    }
}

/// Registered only when the node runs with `--dangerously-allow-agent-pairing`.
#[tool_router(router = pairing_tools)]
impl DashbeamMcp {
    #[tool(
        description = "Sends a pair request to a nearby device (name or endpoint id from list_nearby). Pairing adds a device to the user's trusted network; the other device must accept. Available only because the node was started with --dangerously-allow-agent-pairing."
    )]
    async fn pair_with_device(&self, Parameters(p): Parameters<DeviceParams>) -> CallToolResult {
        if !self.pairing_allowed {
            return err(anyhow::anyhow!("agent pairing is not allowed"));
        }
        respond(
            async {
                let delivered = self
                    .call(Request::AgentPairNearby { device: p.device })
                    .await?;
                Ok(json!({ "delivered": delivered }))
            }
            .await,
        )
    }

    #[tool(
        description = "Marks a paired device trusted, so its files are received without asking, or untrusted with trusted: false. Available only because the node was started with --dangerously-allow-agent-pairing."
    )]
    async fn trust_device(&self, Parameters(p): Parameters<TrustParams>) -> CallToolResult {
        if !self.pairing_allowed {
            return err(anyhow::anyhow!("agent pairing is not allowed"));
        }
        respond(
            self.call(Request::AgentTrust {
                device: p.device,
                trusted: p.trusted.unwrap_or(true),
            })
            .await,
        )
    }
}

#[tool_handler(router = self.tool_router, name = "dashbeam", instructions = "DashBeam moves files between this machine's DashBeam agent node and the user's paired devices (their trusted network). Sending to a paired device needs no confirmation here; trusted devices receive automatically, others must accept. One-time tickets can be shared with anyone. Received files land in the node's receive folder and are never opened. This server cannot pair devices or change trust unless the node was started with --dangerously-allow-agent-pairing.")]
impl ServerHandler for DashbeamMcp {}

/// `dashbeam mcp`: serves MCP on stdin/stdout until the client disconnects.
pub async fn serve_stdio(data_dir: &Path) -> Result<()> {
    let server = DashbeamMcp::connect(data_dir).await?;
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_timeout_caps_at_120() {
        assert_eq!(clamp_timeout(Some(9999)), 120);
        assert_eq!(clamp_timeout(Some(120)), 120);
        assert_eq!(clamp_timeout(Some(5)), 5);
        assert_eq!(clamp_timeout(Some(0)), 0);
        assert_eq!(clamp_timeout(None), DEFAULT_WAIT_SECS);
    }
}
