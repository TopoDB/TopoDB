//! Shared JSON-RPC-over-stdio test harness for `topodb-mcp` integration
//! tests (`smoke.rs`, `e2e.rs`).
//!
//! Framing: rmcp's stdio transport (verified against rmcp 2.2.0's
//! `transport::async_rw`) reads with `read_until(b'\n')` and writes one JSON
//! object per line — so every message here is a single `\n`-terminated
//! line of JSON, no Content-Length headers.
//!
//! Windows-safety / no-hang guarantees:
//! - The child's stdout is read line-by-line on a dedicated background
//!   thread that forwards each line over an `mpsc` channel. A *blocking*
//!   read on the child's pipe directly on the test thread would hang the
//!   whole test forever if the server never responds (no way to time out a
//!   blocking `Read::read`); routing through a channel lets `recv_timeout`
//!   enforce a real deadline instead.
//! - Every read off that channel (`Server::recv`) takes an explicit
//!   `Duration` and uses `recv_timeout` — never a bare `recv()`.
//! - `Server::recv_response` re-derives the *remaining* time on every loop
//!   iteration against a fixed deadline (`Instant::now() + timeout` computed
//!   once), so a server that interleaves unrelated notifications can't reset
//!   the clock and turn a 10s deadline into an unbounded wait.
//! - `Drop for Server` kills the child unconditionally (`Child::kill`) and
//!   then `wait()`s on all paths — including test panics/early returns via
//!   Rust's unwind-runs-destructors guarantee — so no test leaves an orphan
//!   `topodb-mcp.exe` behind on Windows.
//! - Nothing here uses Unix-only APIs (no `libc`, no process groups) or an
//!   async runtime; it's plain `std::process` + `std::thread`, portable to
//!   the Windows CI box this crate targets.
//!
//! `cargo` only auto-discovers direct `tests/*.rs` files as integration-test
//! binaries, not files under a subdirectory — so `tests/common/mod.rs` is
//! *not* itself a separate test target; each `tests/*.rs` file that wants it
//! declares `mod common;` and gets these items compiled in as a plain module.
//! That's the standard Rust integration-test convention for sharing a test
//! harness across multiple `tests/*.rs` files without a real `dev-dependency`
//! crate.

#![allow(dead_code)] // Not every helper is used by every test binary that includes this module.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The deadline every helper here defaults to when a test doesn't need a
/// tighter one. Generous enough to absorb slow CI process-spawn/IO
/// scheduling, tight enough that a genuinely hung server still fails the
/// test instead of the test runner's own timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Owns a spawned `topodb-mcp` child process and a background thread reading
/// its stdout line-by-line, so every read from the child can enforce a
/// deadline (see module docs). Also tracks the next JSON-RPC request id so
/// callers don't have to hand-thread incrementing ids through a whole
/// scenario.
pub struct Server {
    child: Child,
    // Option so `close_stdin_and_wait_exit` can drop it (EOF) while the
    // Server value stays alive for the exit wait.
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    _reader: thread::JoinHandle<()>,
    next_id: i64,
}

impl Server {
    /// Spawns `CARGO_BIN_EXE_topodb-mcp --db <db_path> <extra_args...>` with
    /// piped stdin/stdout and inherited stderr (so a crashing server's panic
    /// message shows up in `cargo test` output instead of being swallowed).
    pub fn spawn(db_path: &Path, extra_args: &[&str]) -> Self {
        Self::spawn_with_env(db_path, extra_args, &[])
    }

    /// Like [`Server::spawn`], but with extra environment variables — the seam
    /// for tests that must control the embedder's ONNX Runtime discovery
    /// (`ORT_DYLIB_PATH`) without depending on the host machine's state.
    pub fn spawn_with_env(db_path: &Path, extra_args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"));
        cmd.arg("--db").arg(db_path);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Keep every test server offline by default: `--embeddings off` goes
        // BEFORE `extra_args` because the config parse loop is last-wins, so
        // a test that wants to exercise the real embedder passes its own
        // `--embeddings`/`--model-dir` later and it still wins. Without this,
        // every server spawned by every existing test would kick off a
        // background model download.
        cmd.arg("--embeddings").arg("off");
        for arg in extra_args {
            cmd.arg(arg);
        }
        let mut child = cmd
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
            stdin: Some(stdin),
            lines: rx,
            _reader: reader,
            next_id: 0,
        }
    }

    /// Next JSON-RPC request id, starting at 1 and incrementing per call.
    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    /// Writes one JSON-RPC message as a single `\n`-terminated line.
    fn send(&mut self, msg: &serde_json::Value) {
        let mut line = serde_json::to_string(msg).expect("serialize");
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        stdin.write_all(line.as_bytes()).expect("write stdin");
        stdin.flush().expect("flush stdin");
    }

    /// Reads the next stdout line as JSON, failing if none arrives before
    /// the deadline (never a bare blocking read — see module docs).
    fn recv(&self, timeout: Duration) -> serde_json::Value {
        let line = self
            .lines
            .recv_timeout(timeout)
            .expect("timed out waiting for a response line from the server");
        serde_json::from_str(&line).expect("parse response line as JSON")
    }

    /// Reads response lines until one matches `id` (skipping any
    /// notifications the server may interleave), with an overall deadline
    /// that does not reset on each interleaved notification.
    fn recv_response(&self, id: i64, timeout: Duration) -> serde_json::Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for matching response id");
            let msg = self.recv(remaining);
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return msg;
            }
            // Otherwise it's a notification or an unrelated message; keep reading.
        }
    }

    /// Performs the `initialize` request + `notifications/initialized`
    /// handshake every MCP session needs before any other call, and returns
    /// the `initialize` response's `result` for the caller to assert on.
    pub fn initialize(&mut self, timeout: Duration) -> serde_json::Value {
        self.initialize_with_version("2025-11-25", timeout)
    }

    /// Like [`initialize`], but sends a caller-chosen `protocolVersion`. Real
    /// MCP clients pin different versions (Claude/Pi/Codex have shipped
    /// `2024-11-05`, `2025-03-26`, and `2025-06-18` at various points), so a
    /// test can drive the handshake with each to confirm the server negotiates
    /// rather than hard-failing on anything but one string.
    pub fn initialize_with_version(
        &mut self,
        version: &str,
        timeout: Duration,
    ) -> serde_json::Value {
        let id = self.next_id();
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": version,
                "capabilities": {},
                "clientInfo": { "name": "topodb-mcp-tests", "version": "0" }
            }
        }));
        let resp = self.recv_response(id, timeout);
        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("initialize should return a result: {resp}"))
            .clone();

        // No response is expected for this notification (JSON-RPC
        // notifications carry no `id`).
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));

        result
    }

    /// Calls `tools/list` and returns the raw `tools` array.
    pub fn tools_list(&mut self, timeout: Duration) -> Vec<serde_json::Value> {
        let id = self.next_id();
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list"
        }));
        let resp = self.recv_response(id, timeout);
        resp.get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .unwrap_or_else(|| panic!("tools/list result should contain a tools array: {resp}"))
            .clone()
    }

    /// Sends a `tools/call` for `name` with `arguments` and returns the raw
    /// JSON-RPC response — both success and error shapes. Callers pick the
    /// assertion they need: [`structured_content`] for the happy path,
    /// [`expect_tool_error`] for the error paths.
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> serde_json::Value {
        let id = self.next_id();
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
        self.recv_response(id, timeout)
    }

    /// Convenience wrapper around [`Server::call_tool`] +
    /// [`structured_content`] for the common case of a call that's expected
    /// to succeed.
    pub fn call_tool_ok(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> serde_json::Value {
        let resp = self.call_tool(name, arguments, timeout);
        structured_content(&resp)
    }
}

impl Server {
    /// Closes the server's stdin (delivering EOF) and waits up to `timeout`
    /// for the process to exit; `true` iff it exited. The regression seam
    /// for "a server whose parent went away must actually terminate" — a
    /// leaked server holds the redb lock and wedges every later open of the
    /// same db (the 0.0.9 broker idle-exit failure).
    pub fn close_stdin_and_wait_exit(&mut self, timeout: std::time::Duration) -> bool {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(_) => return false,
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Closing stdin lets the server exit cleanly on EOF; kill as a
        // backstop so a hung or misbehaving server never survives a test
        // (panic, early return, or normal completion all run this).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Asserts a `tools/call` response ([`Server::call_tool`]'s return value)
/// succeeded and returns its `structuredContent`.
pub fn structured_content(resp: &serde_json::Value) -> serde_json::Value {
    assert!(
        resp.get("error").is_none(),
        "tools/call should not be a JSON-RPC protocol error: {resp:#?}"
    );
    let result = resp
        .get("result")
        .unwrap_or_else(|| panic!("tools/call should return a result: {resp:#?}"));
    assert_ne!(
        result.get("isError").and_then(|v| v.as_bool()),
        Some(true),
        "tools/call should not be a tool error: {result:#?}"
    );
    result
        .get("structuredContent")
        .unwrap_or_else(|| panic!("tools/call should carry structuredContent: {result:#?}"))
        .clone()
}

/// Asserts a `tools/call` response ([`Server::call_tool`]'s return value) is
/// a clean tool-level error — not a crash (no panic/EOF on the child side,
/// which would instead show up as a timeout or a parse failure upstream in
/// [`Server::recv`]) and not a normal success result.
///
/// Accepts either shape rmcp 2.2.0 can produce for a `#[tool]` fn's
/// `Err(ErrorData)`:
/// - a top-level JSON-RPC `error` object — what `Result<Json<T>,
///   ErrorData>`'s `IntoCallToolResult` impl actually produces for a tool
///   body's own `Err` (it degrades straight through to the protocol-level
///   `Err` case; see `rmcp::handler::server::router::tool`'s blanket
///   `impl<T, E: IntoCallToolResult> IntoCallToolResult for Result<T, E>`
///   together with `impl IntoCallToolResult for ErrorData`), or
/// - `result.isError: true` — the one case rmcp special-cases differently:
///   tool *argument deserialization* failures (`into_tool_argument_error`),
///   which never reach our tool bodies at all.
///
/// Every Task 6 error path (undeclared-prop lookup, bogus link endpoint,
/// malformed scope string) is business logic inside the tool body, so in
/// practice these land on the first branch — but asserting both keeps this
/// helper correct if a future tool routes a bad-input case through argument
/// deserialization instead.
pub fn expect_tool_error(resp: &serde_json::Value) {
    let is_protocol_error = resp.get("error").is_some();
    let is_result_error = resp
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|v| v.as_bool())
        == Some(true);
    assert!(
        is_protocol_error || is_result_error,
        "expected a tool error (JSON-RPC `error` or `result.isError: true`), got: {resp:#?}"
    );
}
