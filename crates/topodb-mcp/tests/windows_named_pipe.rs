#![cfg(windows)]

//! Windows named-pipe transport smoke test.
//!
//! The unix daemon coverage (multiprocess.rs) is unix-socket-based, so this is
//! the one place the WINDOWS transport is exercised end to end: spawn a real
//! `topodb-mcp --socket <pipe>` daemon, connect a tokio named-pipe client,
//! complete the hello + MCP handshake, write a memory and read it back. It runs
//! only on the CI `test (windows-latest)` job.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

/// Kills the daemon on drop so a failed assertion never leaks a lock holder.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Open the pipe, retrying while the daemon is still binding it (a not-yet-
/// created pipe yields `NotFound`; a busy instance yields `PIPE_BUSY`).
async fn connect_with_retry(pipe: &str) -> NamedPipeClient {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(client) => return client,
            Err(e)
                if e.raw_os_error() == Some(2 /* ERROR_FILE_NOT_FOUND */)
                    || e.raw_os_error() == Some(231 /* ERROR_PIPE_BUSY */) =>
            {
                if Instant::now() > deadline {
                    panic!("named pipe {pipe} never became connectable: {e}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("opening named pipe {pipe}: {e}"),
        }
    }
}

#[tokio::test]
async fn windows_daemon_serves_over_a_named_pipe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("t.redb");
    // A unique explicit pipe name (the daemon accepts --socket PATH verbatim).
    let pipe = format!(r"\\.\pipe\topodb-test-{}", std::process::id());

    let child = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
        .arg("--socket")
        .arg(&pipe)
        .arg("--db")
        .arg(&db)
        .arg("--embeddings")
        .arg("off")
        .spawn()
        .expect("spawn daemon");
    let _guard = DaemonGuard(child);

    let client = connect_with_retry(&pipe).await;
    let (read_half, mut write_half) = tokio::io::split(client);
    let mut reader = BufReader::new(read_half);

    // Newline-delimited JSON-RPC framing.
    let frame = |v: serde_json::Value| {
        let mut s = v.to_string();
        s.push('\n');
        s
    };
    async fn read_reply(
        reader: &mut BufReader<tokio::io::ReadHalf<NamedPipeClient>>,
        want_id: i64,
    ) -> serde_json::Value {
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.expect("read");
            assert!(
                n > 0,
                "daemon closed the pipe before replying to id {want_id}"
            );
            let msg: serde_json::Value = serde_json::from_str(line.trim()).expect("json line");
            if msg.get("id").and_then(|v| v.as_i64()) == Some(want_id) {
                return msg;
            }
        }
    }

    // Hello (scope stamp) MUST come first, before any JSON-RPC.
    write_half
        .write_all(frame(serde_json::json!({ "topodb/hello": { "scope": "shared", "read_scopes": ["shared"] } })).as_bytes())
        .await
        .expect("write hello");
    // initialize / initialized
    write_half
        .write_all(
            frame(serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": { "name": "pipe-test", "version": "0" } }
            }))
            .as_bytes(),
        )
        .await
        .expect("write initialize");
    let init = read_reply(&mut reader, 1).await;
    assert!(init.get("error").is_none(), "initialize failed: {init}");
    write_half
        .write_all(frame(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} })).as_bytes())
        .await
        .expect("write initialized");

    // Write a memory, then search it back — proving reads and writes both flow
    // over the named pipe against the shared Db.
    write_half
        .write_all(
            frame(serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "remember", "arguments": { "content": "windows pipe fact", "entities": ["Pipe"] } }
            }))
            .as_bytes(),
        )
        .await
        .expect("write remember");
    let remembered = read_reply(&mut reader, 2).await;
    assert!(
        remembered.get("error").is_none(),
        "remember failed: {remembered}"
    );

    write_half
        .write_all(
            frame(serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "search_memories", "arguments": { "query": "windows pipe", "k": 5 } }
            }))
            .as_bytes(),
        )
        .await
        .expect("write search");
    let found = read_reply(&mut reader, 3).await;
    assert!(found.get("error").is_none(), "search failed: {found}");
    let hits = &found["result"]["structuredContent"]["hits"];
    assert!(
        hits.as_array().is_some_and(|a| !a.is_empty()),
        "search over the named pipe should recall the memory: {found}"
    );
}
