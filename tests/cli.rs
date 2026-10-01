use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use strata::{Append, protocol};
use tempfile::{TempDir, tempdir};

const BIN: &str = env!("CARGO_BIN_EXE_strata");
struct Daemon {
    child: Child,
    dir: TempDir,
    socket: PathBuf,
    runtime: TempDir,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Daemon {
    fn start() -> Self {
        let dir = tempdir().unwrap();
        // macOS Unix sockets have short path limits; don't use the long TMPDIR.
        let runtime = tempfile::Builder::new()
            .prefix("strata-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = runtime.path().join("s");
        let child = spawn(dir.path(), &socket);
        let mut result = Self {
            child,
            dir,
            socket,
            runtime,
        };
        result.ready();
        result
    }
    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon exited before ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon not ready");
    }
    fn stop(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
    fn restart(&mut self) {
        self.child = spawn(self.dir.path(), &self.socket);
        self.ready();
    }
}
fn spawn(dir: &Path, socket: &Path) -> Child {
    Command::new(BIN)
        .arg("serve")
        .arg("--dir")
        .arg(dir)
        .arg("--socket")
        .arg(socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}
fn request(id: &str) -> Append {
    Append {
        id: id.into(),
        kind: "client.food".into(),
        data: json!({"content":"steak and fries"}),
    }
}
fn wire(socket: &Path, bytes: &[u8]) -> Value {
    let mut connection = UnixStream::connect(socket).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    connection.write_all(bytes).unwrap();
    let mut reply = String::new();
    connection.read_to_string(&mut reply).unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[test]
fn cli_stdin_receipt_searchable_file_and_restart() {
    let mut daemon = Daemon::start();
    let mut command = Command::new(BIN)
        .args(["append", "--type", "client.food", "--id", "food-1"])
        .env("STRATA_SOCKET", &daemon.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    command
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"client":"alice","content":"steak and fries"}"#)
        .unwrap();
    let output = command.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["sequence"], 1);
    let text =
        fs::read_to_string(daemon.dir.path().join(receipt["file"].as_str().unwrap())).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains("steak and fries"));
    assert_eq!(
        fs::metadata(&daemon.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    daemon.stop();
    daemon.restart();
    let retry = Append {
        id: "food-1".into(),
        kind: "client.food".into(),
        data: json!({"client":"alice","content":"steak and fries"}),
    };
    assert_eq!(
        protocol::append(&daemon.socket, &retry).unwrap().sequence,
        1
    );
    daemon.stop();
    let out = Command::new(BIN)
        .arg("verify")
        .arg("--dir")
        .arg(daemon.dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn concurrent_clients_and_duplicate_requests_are_serialized() {
    let mut daemon = Daemon::start();
    let receipts = thread::scope(|scope| {
        let mut jobs = vec![];
        for n in 0..24 {
            let socket = &daemon.socket;
            jobs.push(scope.spawn(move || {
                protocol::append(socket, &request(&format!("event-{}", n % 12))).unwrap()
            }));
        }
        jobs.into_iter()
            .map(|j| j.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut sequences: Vec<_> = receipts.iter().map(|r| r.sequence).collect();
    sequences.sort();
    sequences.dedup();
    assert_eq!(sequences, (1..=12).collect::<Vec<_>>());
    daemon.stop();
    assert_eq!(strata::verify(daemon.dir.path(), None).unwrap().events, 12);
}

#[test]
fn disconnected_client_can_retry_without_duplicate() {
    let mut daemon = Daemon::start();
    let req = request("lost-response");
    let mut connection = UnixStream::connect(&daemon.socket).unwrap();
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    connection.write_all(&bytes).unwrap();
    drop(connection);
    let receipt = protocol::append(&daemon.socket, &req).unwrap();
    assert_eq!(receipt.sequence, 1);
    daemon.stop();
    assert_eq!(strata::verify(daemon.dir.path(), None).unwrap().events, 1);
}

#[test]
fn malformed_unknown_and_oversized_requests_do_not_break_server() {
    let mut daemon = Daemon::start();
    for input in [
        b"not json\n".to_vec(),
        b"{\"id\":\"x\",\"type\":\"food\",\"data\":{},\"extra\":true}\n".to_vec(),
        [vec![b'x'; strata::record::MAX_REQUEST + 1], vec![b'\n']].concat(),
    ] {
        assert!(wire(&daemon.socket, &input)["error"].is_string());
    }
    assert_eq!(
        protocol::append(&daemon.socket, &request("valid"))
            .unwrap()
            .sequence,
        1
    );
    daemon.stop();
}

#[test]
fn second_daemon_cannot_take_store_or_socket() {
    let mut daemon = Daemon::start();
    let other = tempdir().unwrap();
    for (dir, socket) in [
        (daemon.dir.path(), daemon.runtime.path().join("other")),
        (other.path(), daemon.socket.clone()),
    ] {
        let out = Command::new(BIN)
            .arg("serve")
            .arg("--dir")
            .arg(dir)
            .arg("--socket")
            .arg(socket)
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
    assert!(protocol::append(&daemon.socket, &request("still-live")).is_ok());
    daemon.stop();
}

#[test]
fn refuses_to_replace_regular_file_or_symlink_with_socket() {
    for link in [false, true] {
        let dir = tempdir().unwrap();
        let runtime = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let socket = runtime.path().join("s");
        let target = runtime.path().join("target");
        fs::write(&target, b"keep").unwrap();
        if link {
            symlink(&target, &socket).unwrap();
        } else {
            fs::write(&socket, b"keep").unwrap();
        }
        let out = Command::new(BIN)
            .arg("serve")
            .arg("--dir")
            .arg(dir.path())
            .arg("--socket")
            .arg(&socket)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert_eq!(fs::read(socket).unwrap(), b"keep");
    }
}

#[test]
fn generated_id_is_reported_even_when_daemon_unavailable() {
    let mut child = Command::new(BIN)
        .args([
            "append",
            "--socket",
            "/nonexistent-strata-socket",
            "--type",
            "food",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("request id"));
}

#[test]
fn independent_python_generated_format_vector() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("2026-09-30.jsonl"),
        include_bytes!("fixtures/2026-09-30.jsonl"),
    )
    .unwrap();
    let status = strata::verify(dir.path(), None).unwrap();
    assert_eq!(status.events, 1);
}
