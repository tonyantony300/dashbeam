//! The MCP server driven in-process over a duplex transport against a node
//! in a temp data dir. Binds real endpoints: run with `--test-threads=1`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use agent::approver::DenyAll;
use agent::config::AgentConfig;
use agent::service::{AgentOptions, AgentService};
use dashbeam_cli::ipc::{self, StartedBy};
use dashbeam_cli::mcp::DashbeamMcp;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::sync::oneshot;

const BASE_TOOLS: [&str; 17] = [
    "get_identity",
    "list_devices",
    "list_nearby",
    "send_to_device",
    "share_ticket",
    "receive_ticket",
    "transfer_status",
    "wait_for_transfer",
    "cancel_transfer",
    "list_transfers",
    "wait_for_incoming",
    "list_incoming",
    "accept_incoming",
    "decline_incoming",
    "send_message",
    "wait_for_message",
    "list_messages",
];
const PAIRING_TOOLS: [&str; 2] = ["pair_with_device", "trust_device"];

async fn start_agent(dir: &Path, allow_agent_pairing: bool) -> Arc<AgentService> {
    AgentService::start(AgentOptions {
        data_dir: dir.to_path_buf(),
        config: AgentConfig::default(),
        allow_agent_pairing,
        discoverable_override: false,
        approver: Arc::new(DenyAll),
    })
    .await
    .expect("agent start")
}

/// A node serving its socket in-process, plus an MCP client connected to a
/// `DashbeamMcp` over a duplex pipe.
struct Fixture {
    svc: Arc<AgentService>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    client: Option<RunningService<RoleClient, ()>>,
    mcp: Option<tokio::task::JoinHandle<()>>,
    dir: tempfile::TempDir,
}

impl Fixture {
    async fn start(allow_agent_pairing: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let svc = start_agent(dir.path(), allow_agent_pairing).await;
        let server = ipc::Server::bind(svc.clone(), StartedBy::Hand).expect("bind");
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(server.run(async move {
            let _ = rx.await;
        }));

        let mcp_server = DashbeamMcp::connect(dir.path()).await.expect("mcp connect");
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let mcp = tokio::spawn(async move {
            if let Ok(running) = mcp_server.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        let client = ().serve(client_io).await.expect("mcp client");
        Self {
            svc,
            stop: Some(tx),
            server: Some(handle),
            client: Some(client),
            mcp: Some(mcp),
            dir,
        }
    }

    fn client(&self) -> &RunningService<RoleClient, ()> {
        self.client.as_ref().unwrap()
    }

    async fn tool_names(&self) -> BTreeSet<String> {
        self.client()
            .list_all_tools()
            .await
            .expect("tools/list")
            .into_iter()
            .map(|t| t.name.into_owned())
            .collect()
    }

    async fn call(&self, name: &'static str, args: Value) -> CallToolResult {
        let mut params = CallToolRequestParams::new(name);
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        self.client().call_tool(params).await.expect("tools/call")
    }

    /// Calls a tool that must succeed and returns its JSON result.
    async fn ok(&self, name: &'static str, args: Value) -> Value {
        let res = self.call(name, args).await;
        assert_ne!(res.is_error, Some(true), "{name} failed: {}", text(&res));
        serde_json::from_str(&text(&res)).expect("JSON result")
    }

    async fn stop(mut self) {
        if let Some(c) = self.client.take() {
            let _ = c.cancel().await;
        }
        if let Some(h) = self.mcp.take() {
            let _ = h.await;
        }
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.server.take() {
            h.await.unwrap().unwrap();
        }
        self.svc.shutdown().await.unwrap();
        assert!(!agent::paths::socket_path(self.dir.path()).exists());
    }
}

fn text(res: &CallToolResult) -> String {
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_list_excludes_pairing_tools_without_flag() {
    let fx = Fixture::start(false).await;
    assert_eq!(fx.tool_names().await, names(&BASE_TOOLS));

    // Not routable either, whatever the client sends.
    let err = fx
        .client()
        .call_tool(
            CallToolRequestParams::new("trust_device")
                .with_arguments(json!({"device": "x"}).as_object().unwrap().clone()),
        )
        .await;
    let err = err.expect_err("trust_device must not be routable");
    assert!(err.to_string().contains("tool not found"), "{err}");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_list_includes_pairing_tools_with_flag() {
    let fx = Fixture::start(true).await;
    let mut want = names(&BASE_TOOLS);
    want.extend(names(&PAIRING_TOOLS));
    assert_eq!(want.len(), 19);
    assert_eq!(fx.tool_names().await, want);

    // Routed to the gated node op: an unknown device is a tool error, not "tool not found".
    let res = fx
        .call("trust_device", json!({"device": "no-such-device"}))
        .await;
    assert_eq!(res.is_error, Some(true));
    assert!(text(&res).contains("no-such-device"), "{}", text(&res));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_descriptions_state_the_trust_model() {
    let fx = Fixture::start(false).await;
    let tools = fx.client().list_all_tools().await.unwrap();
    let desc = |n: &str| {
        tools
            .iter()
            .find(|t| t.name == n)
            .and_then(|t| t.description.clone())
            .unwrap_or_default()
            .into_owned()
    };
    assert!(desc("share_ticket")
        .contains("Anyone holding this ticket can download the files once, until it expires."));
    assert!(desc("send_to_device").contains(
        "Sends to one of your paired devices. Trusted devices receive automatically; others must accept. Returns immediately; use wait_for_transfer for the outcome. 'expired' can also mean a busy receiver did not get to it in time."
    ));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_identity_returns_agent_device() {
    let fx = Fixture::start(false).await;
    let me = fx.ok("get_identity", json!({})).await;
    assert_eq!(me["device_type"], "agent");
    assert_eq!(me["endpoint_id"], fx.svc.identity().endpoint_id);
    assert_eq!(me["display_name"], fx.svc.identity().display_name);
    assert_eq!(me["online"], true);
    assert_eq!(me["agent_pairing_allowed"], false);

    let devices = fx.ok("list_devices", json!({})).await;
    assert_eq!(devices, json!([]));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_errors_are_tool_errors() {
    let fx = Fixture::start(false).await;
    let res = fx
        .call("transfer_status", json!({"transfer_id": "nope-123"}))
        .await;
    assert_eq!(res.is_error, Some(true));
    assert!(text(&res).contains("nope-123"), "{}", text(&res));

    // A missing path fails before anything is shared, naming the resolved path.
    let res = fx
        .call(
            "share_ticket",
            json!({"paths": ["definitely-missing-file.txt"]}),
        )
        .await;
    assert_eq!(res.is_error, Some(true));
    let cwd = std::env::current_dir().unwrap();
    let want = cwd.join("definitely-missing-file.txt");
    assert!(
        text(&res).contains(&want.display().to_string()),
        "{}",
        text(&res)
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_for_incoming_cursor_advances() {
    let fx = Fixture::start(false).await;
    let sender_dir = tempfile::tempdir().unwrap();
    let sender = start_agent(sender_dir.path(), false).await;
    let src = tempfile::tempdir().unwrap();

    let mut received = Vec::new();
    for name in ["one.txt", "two.txt"] {
        let file = src.path().join(name);
        std::fs::write(&file, name.as_bytes()).unwrap();
        let shared = sender
            .share_ticket(vec![file], src.path().to_path_buf(), None)
            .await
            .unwrap();
        let ticket = shared.ticket.expect("ticket");
        // Returns at once; the outcome comes from the wait tools.
        let rec = fx.ok("receive_ticket", json!({"ticket": ticket})).await;
        assert_eq!(rec["status"], "transferring", "{rec}");
        let id = rec["id"].as_str().unwrap().to_string();
        assert_eq!(rec["transfer_id"], rec["id"], "{rec}");

        let batch = fx
            .ok("wait_for_incoming", json!({"timeout_secs": 60}))
            .await;
        let ids: Vec<&str> = batch["transfers"]
            .as_array()
            .expect("transfers")
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        // Only the transfer received since the last call; no repeats.
        assert_eq!(ids, vec![id.as_str()], "{batch}");
        assert!(batch["transfers"][0]["saved_paths"][0].is_string());
        received.push(id);
    }

    let batch = fx.ok("wait_for_incoming", json!({"timeout_secs": 1})).await;
    assert_eq!(batch["transfers"], json!([]), "{batch}");

    let list = fx
        .ok("list_transfers", json!({"direction": "inbound"}))
        .await;
    assert_eq!(list.as_array().unwrap().len(), 2);

    sender.shutdown().await.unwrap();
    fx.stop().await;
}
