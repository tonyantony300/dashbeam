use crate::history::HistoryRecordingEmitter;
use engine::{NodeService, SendResult};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Application state for managing sharing sessions
pub struct AppState {
    pub node: Option<Arc<NodeService>>,
    /// Set when device-node initialization fails (pairing unavailable; send/receive still work).
    pub node_init_error: Option<String>,
    pub current_share: Option<ShareHandle>,
    pub is_share_starting: bool,
    pub is_transporting: bool,
    pub launch_intent: Option<String>,
    pub current_receive_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Hash of the receive in flight, so history cannot delete the partial
    /// store out from under an active download.
    pub current_receive_hash: Option<String>,
    /// Hash and destination of the last receive that left a partial store
    /// behind; together they locate it.
    pub last_cancelled_recv: Option<(String, std::path::PathBuf)>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            node: None,
            node_init_error: None,
            current_share: None,
            is_share_starting: false,
            is_transporting: false,
            launch_intent: None,
            current_receive_cancel: None,
            current_receive_hash: None,
            last_cancelled_recv: None,
        }
    }
}

/// Handle for an active sharing session
/// CRITICAL: This struct holds the router and temp_tag which keeps the server alive
pub struct ShareHandle {
    pub ticket: String,
    pub _path: PathBuf,
    pub send_result: SendResult,
    /// Present only while history recording is enabled. Held here so
    /// `stop_sharing` can close the row a broadcast share left open.
    pub recorder: Option<Arc<HistoryRecordingEmitter>>,
}

impl ShareHandle {
    pub fn new(
        ticket: String,
        path: PathBuf,
        send_result: SendResult,
        recorder: Option<Arc<HistoryRecordingEmitter>>,
    ) -> Self {
        Self {
            ticket,
            _path: path,
            send_result,
            recorder,
        }
    }

    /// Stop the sharing session and free its resources.
    pub async fn stop(&mut self) -> Result<(), String> {
        use std::time::Duration;

        match tokio::time::timeout(Duration::from_secs(2), self.send_result.router.shutdown()).await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("Router shutdown error: {}", e);
            }
            Err(_) => {
                tracing::warn!("Router shutdown timeout after 2 seconds");
            }
        }

        let endpoint = self.send_result.router.endpoint();
        endpoint.close().await;

        Ok(())
    }
}

pub type AppStateMutex = Arc<Mutex<AppState>>;
