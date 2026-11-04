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
    unsafe { libc::kill(pid, 0) == 0 }
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
                unsafe { libc::kill(pid, libc::SIGKILL) };
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

    unsafe { libc::kill(pid, libc::SIGKILL) };
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
    assert!(stderr(&o).contains("/nope"), "{}", stderr(&o));
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
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let node = Node::new();
    let o = dashbeam(node.path(), &["id"]);
    assert!(o.status.success(), "id failed: {}", stderr(&o));
    let id = stdout(&o).trim().to_string();
    let st = status_json(node.path()).unwrap();
    let old_pid = st["pid"].as_i64().unwrap() as i32;
    let sock = std::path::PathBuf::from(st["data_dir"].as_str().unwrap()).join("node.sock");

    // Shutdown removes the socket at once but holds the lock while the node winds down.
    let mut s = UnixStream::connect(&sock).unwrap();
    s.write_all(b"{\"hello\":{\"version\":1}}\n{\"op\":\"shutdown\"}\n")
        .unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    line.clear();
    r.read_line(&mut line).unwrap();
    assert!(line.contains("\"ok\""), "{line}");
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
