//! Drives the built `dashbeam` binary against a temp data dir. Starts real
//! nodes: run with `--test-threads=1`.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_dashbeam");

fn dashbeam(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .env_remove("DASHBEAM_AGENT_DIR")
        .stdin(Stdio::null())
        .output()
        .expect("run dashbeam")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn status_json(dir: &Path) -> Option<serde_json::Value> {
    let o = dashbeam(dir, &["--json", "status"]);
    o.status
        .success()
        .then(|| serde_json::from_slice(&o.stdout).expect("status json"))
}

fn alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(windows)]
    {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")),
            Err(_) => false,
        }
    }
}

fn force_kill(pid: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Owns a temp data dir and makes sure no daemon outlives the test, even
/// when an assertion fails first.
struct Node {
    dir: tempfile::TempDir,
}

impl Node {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let pid = status_json(self.path()).and_then(|s| s["pid"].as_i64());
        let _ = dashbeam(self.path(), &["stop"]);
        if let Some(pid) = pid {
            let pid = pid as i32;
            if alive(pid) {
                force_kill(pid);
            }
        }
    }
}

fn is_hex_id(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn id_autostarts_node_and_stop_stops_it() {
    let node = Node::new();
    let o = dashbeam(node.path(), &["id"]);
    assert!(o.status.success(), "id failed: {}", stderr(&o));
    let id = stdout(&o).trim().to_string();
    assert!(is_hex_id(&id), "not a 64-hex id: {id:?}");

    let st = status_json(node.path()).expect("status while running");
    assert_eq!(st["started_by"], "auto", "{st}");
    assert_eq!(st["node"]["endpoint_id"], id.as_str(), "{st}");
    let pid = st["pid"].as_i64().expect("pid") as i32;

    let o = dashbeam(node.path(), &["stop"]);
    assert!(o.status.success(), "stop failed: {}", stderr(&o));
    assert!(!alive(pid), "daemon still alive after stop");

    let o = dashbeam(node.path(), &["status"]);
    assert!(!o.status.success(), "status succeeded with no node");
}

#[test]
fn stale_socket_after_kill_recovers() {
    let node = Node::new();
    let o = dashbeam(node.path(), &["id"]);
    assert!(o.status.success(), "id failed: {}", stderr(&o));
    let id = stdout(&o).trim().to_string();
    let pid = status_json(node.path()).unwrap()["pid"].as_i64().unwrap() as i32;

    force_kill(pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid));
    assert!(
        agent::paths::socket_path(node.path()).exists(),
        "kill -9 leaves the socket file behind"
    );

    let o = dashbeam(node.path(), &["id"]);
    assert!(o.status.success(), "id after kill failed: {}", stderr(&o));
    assert_eq!(stdout(&o).trim(), id);
    let new_pid = status_json(node.path()).unwrap()["pid"].as_i64().unwrap() as i32;
    assert_ne!(new_pid, pid);
}

#[test]
fn second_up_on_same_dir_exits_already_running() {
    let node = Node::new();
    let o = dashbeam(node.path(), &["id"]);
    assert!(o.status.success(), "id failed: {}", stderr(&o));

    let o = dashbeam(node.path(), &["up"]);
    assert_eq!(o.status.code(), Some(2), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("already running"), "{}", stderr(&o));
    // The first node is untouched.
    assert!(status_json(node.path()).is_some());
}

#[test]
fn send_missing_path_errors_before_contacting_peer() {
    let node = Node::new();
    let o = dashbeam(node.path(), &["send", "somebody", "/nope"]);
    assert!(!o.status.success());
    let err = stderr(&o);
    // `/nope` is `C:\nope` on Windows; either form names the missing path.
    assert!(err.contains("nope"), "{err}");
    // Local validation fails before any node is started.
    assert!(status_json(node.path()).is_none());
}

#[test]
fn relative_share_path_resolves_against_client_cwd() {
    let node = Node::new();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("note.txt"), b"hello").unwrap();
    let o = Command::new(BIN)
        .current_dir(work.path())
        .arg("--data-dir")
        .arg(node.path())
        .args(["--json", "share", "note.txt", "--ttl", "60"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "share failed: {}", stderr(&o));
    let rec: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(rec["file_names"][0], "note.txt", "{rec}");
    assert!(
        rec["ticket"].as_str().is_some_and(|t| !t.is_empty()),
        "{rec}"
    );
}

#[test]
fn autostart_during_node_shutdown_window_succeeds() {
    let node = Node::new();
    let o = dashbeam(node.path(), &["id"]);
    assert!(o.status.success(), "id failed: {}", stderr(&o));
    let id = stdout(&o).trim().to_string();
    let st = status_json(node.path()).unwrap();
    let old_pid = st["pid"].as_i64().unwrap() as i32;
    let sock = std::path::PathBuf::from(st["data_dir"].as_str().unwrap()).join("node.sock");

    // Shutdown removes the socket at once but holds the lock while the node winds down.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = dashbeam_cli::ipc::transport::connect(&sock).await.unwrap();
        let (r, mut w) = dashbeam_cli::ipc::transport::split(stream);
        w.write_all(b"{\"hello\":{\"version\":1}}\n{\"op\":\"shutdown\"}\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(r).lines();
        let _hello = lines.next_line().await.unwrap();
        let reply = lines.next_line().await.unwrap().unwrap();
        assert!(reply.contains("\"ok\""), "{reply}");
    });
    assert!(alive(old_pid), "old node should still be shutting down");

    let started = Instant::now();
    let o = dashbeam(node.path(), &["id"]);
    let took = started.elapsed();
    assert!(
        o.status.success(),
        "id during shutdown failed: {}",
        stderr(&o)
    );
    assert_eq!(stdout(&o).trim(), id);
    assert!(took < Duration::from_secs(20), "took {took:?}");
    let new_pid = status_json(node.path()).unwrap()["pid"].as_i64().unwrap() as i32;
    assert_ne!(new_pid, old_pid);
}

#[test]
fn name_warns_when_config_sets_name() {
    let node = Node::new();
    let o = dashbeam(node.path(), &["name", "Plain Box"]);
    assert!(o.status.success(), "name failed: {}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "Plain Box");
    assert!(!stderr(&o).contains("warning"), "{}", stderr(&o));

    std::fs::write(node.path().join("config.toml"), "name = \"Config Box\"\n").unwrap();
    let o = dashbeam(node.path(), &["name", "Other Box"]);
    assert!(o.status.success(), "name failed: {}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "Other Box");
    assert!(
        stderr(&o).contains("config.toml sets name = \"Config Box\""),
        "{}",
        stderr(&o)
    );
}
