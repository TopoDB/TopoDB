//! End-to-end smoke test: spawn the real `topodb-mcp` binary and drive the MCP
//! initialize handshake + `tools/list` over newline-delimited JSON-RPC (the
//! framing rmcp's stdio transport uses — verified against rmcp 2.2.0's
//! `transport::async_rw`, which reads with `read_until(b'\n')` and writes one
//! JSON object per line).
//!
//! Windows-safe: uses only `std::process` + threaded blocking reads with a
//! deadline (no Unix-only APIs, no async runtime). The child is killed on drop.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Owns the spawned server and reads its stdout line-by-line off a background
/// thread so each read can enforce a deadline (a blocking read on the child's
/// pipe would otherwise hang the test forever if the server misbehaves).
struct Server {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
    _reader: thread::JoinHandle<()>,
}

impl Server {
    fn spawn(db_path: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
            .arg("--db")
            .arg(db_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn topodb-mcp binary");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout: ChildStdout = child.stdout.take().expect("child stdout");

        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buf = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match buf.read_line(&mut line) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Server {
            child,
            stdin,
            lines: rx,
            _reader: reader,
        }
    }

    /// Writes one JSON-RPC message as a single `\n`-terminated line.
    fn send(&mut self, msg: &serde_json::Value) {
        let mut line = serde_json::to_string(msg).expect("serialize");
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).expect("write stdin");
        self.stdin.flush().expect("flush stdin");
    }

    /// Reads the next stdout line as JSON, failing if none arrives before the
    /// deadline.
    fn recv(&self, timeout: Duration) -> serde_json::Value {
        let line = self
            .lines
            .recv_timeout(timeout)
            .expect("timed out waiting for a response line from the server");
        serde_json::from_str(&line).expect("parse response line as JSON")
    }

    /// Reads response lines until one matches `id` (skipping any notifications
    /// the server may interleave), with an overall deadline.
    fn recv_response(&self, id: i64, timeout: Duration) -> serde_json::Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("timed out waiting for matching response id");
            let msg = self.recv(remaining);
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return msg;
            }
            // Otherwise it's a notification or an unrelated message; keep reading.
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Closing stdin lets the server exit cleanly on EOF; kill as a backstop.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_and_tools_list_exposes_db_info() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("smoke.redb");
    let mut server = Server::spawn(&db_path);

    let timeout = Duration::from_secs(10);

    // 1. initialize
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "smoke", "version": "0" }
        }
    }));
    let init = server.recv_response(1, timeout);
    assert!(
        init.get("result")
            .and_then(|r| r.get("capabilities"))
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize result should advertise tools capability: {init}"
    );

    // 2. notifications/initialized (no response expected)
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));

    // 3. tools/list
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    }));
    let list = server.recv_response(2, timeout);

    let tools = list
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools/list result should contain a tools array");

    let db_info = tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("db_info"))
        .expect("db_info tool must be present in tools/list");

    let description = db_info
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    assert!(
        !description.is_empty(),
        "db_info must carry a non-empty description"
    );
}
