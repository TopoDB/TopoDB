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

    // Task 4 added six read tools alongside db_info: db_info, get_node,
    // find_by_prop, search_memories, traverse, access_stats, get_changes.
    // Task 5 added three write tools: create_memory, create_entity, link.
    assert_eq!(
        tools.len(),
        10,
        "expected 10 tools (db_info + 6 read + 3 write), got: {tools:#?}"
    );
    for name in [
        "get_node",
        "find_by_prop",
        "search_memories",
        "traverse",
        "access_stats",
        "get_changes",
        "create_memory",
        "create_entity",
        "link",
    ] {
        assert!(
            tools
                .iter()
                .any(|t| t.get("name").and_then(|n| n.as_str()) == Some(name)),
            "tools/list must include {name}: {tools:#?}"
        );
    }

    // 4. tools/call get_node on a syntactically valid but nonexistent ULID —
    // the fresh tempdir db still has no nodes at this point in the test, so
    // this exercises the clean not-found path, not a crash.
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "get_node",
            "arguments": { "id": "01ARZ3NDEKTSV4RRFFQ69G5FAV" }
        }
    }));
    let call = server.recv_response(3, timeout);
    let result = call
        .get("result")
        .unwrap_or_else(|| panic!("tools/call get_node should return a result: {call}"));
    assert_ne!(
        result.get("isError").and_then(|v| v.as_bool()),
        Some(true),
        "get_node on a nonexistent id must not be a tool error: {result:#?}"
    );
    let structured = result
        .get("structuredContent")
        .expect("get_node result should carry structuredContent");
    assert_eq!(
        structured.get("found"),
        Some(&serde_json::Value::Bool(false)),
        "get_node on a nonexistent id should report found:false, not a crash: {structured:#?}"
    );
    assert!(
        structured.get("node").is_none(),
        "not-found result must not carry a node field: {structured:#?}"
    );

    // --- Task 5 write-tool flow (lean seed of Task 6's full e2e test) ---
    // create_entity -> find_by_prop finds it; create_memory -> search_memories
    // finds it; link -> traverse from the entity reaches the memory.

    // 5. create_entity {name: "ada"}
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "create_entity",
            "arguments": { "name": "ada" }
        }
    }));
    let entity_id = call_tool_ok(&server, 4, timeout)["id"]
        .as_str()
        .expect("create_entity should return a structured id")
        .to_string();

    // 6. find_by_prop should locate the entity by its equality-indexed name.
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "find_by_prop",
            "arguments": { "label": "Entity", "prop": "name", "value": "ada" }
        }
    }));
    let found = call_tool_ok(&server, 5, timeout);
    let nodes = found["nodes"].as_array().expect("nodes array");
    assert!(
        nodes.iter().any(|n| n["id"] == entity_id),
        "find_by_prop should locate the entity just created: {found:#?}"
    );

    // 7. create_memory {content: "ada wrote the first program"}
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "create_memory",
            "arguments": { "content": "ada wrote the first program" }
        }
    }));
    let memory_id = call_tool_ok(&server, 6, timeout)["id"]
        .as_str()
        .expect("create_memory should return a structured id")
        .to_string();

    // 8. search_memories should find it by full-text content.
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "search_memories",
            "arguments": { "query": "ada program" }
        }
    }));
    let hits = call_tool_ok(&server, 7, timeout);
    let hits = hits["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().any(|h| h["node"]["id"] == memory_id),
        "search_memories should find the memory just created: {hits:#?}"
    );

    // 9. link the entity to the memory.
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "link",
            "arguments": {
                "from_id": entity_id,
                "to_id": memory_id,
                "edge_type": "about"
            }
        }
    }));
    let edge_id = call_tool_ok(&server, 8, timeout)["id"]
        .as_str()
        .expect("link should return a structured id")
        .to_string();

    // 10. traverse from the entity should reach the memory via the new edge.
    server.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "traverse",
            "arguments": { "seed_id": entity_id }
        }
    }));
    let subgraph = call_tool_ok(&server, 9, timeout)["subgraph"].clone();
    let sg_nodes = subgraph["nodes"].as_array().expect("subgraph nodes");
    let sg_edges = subgraph["edges"].as_array().expect("subgraph edges");
    assert!(
        sg_nodes.iter().any(|n| n["id"] == memory_id),
        "traverse from the entity should reach the linked memory: {subgraph:#?}"
    );
    assert!(
        sg_edges.iter().any(|e| e["id"] == edge_id),
        "traverse from the entity should surface the link edge: {subgraph:#?}"
    );
}

/// Reads the response for `id`, asserts it isn't a tool error, and returns
/// its `structuredContent` — the shared shape every `tools/call` assertion
/// above needs.
fn call_tool_ok(server: &Server, id: i64, timeout: Duration) -> serde_json::Value {
    let call = server.recv_response(id, timeout);
    let result = call
        .get("result")
        .unwrap_or_else(|| panic!("tools/call (id {id}) should return a result: {call}"));
    assert_ne!(
        result.get("isError").and_then(|v| v.as_bool()),
        Some(true),
        "tools/call (id {id}) should not be a tool error: {result:#?}"
    );
    result
        .get("structuredContent")
        .unwrap_or_else(|| {
            panic!("tools/call (id {id}) should carry structuredContent: {result:#?}")
        })
        .clone()
}
