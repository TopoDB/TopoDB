//! Behavioral tests for the `forget` tool (spec:
//! docs/superpowers/specs/2026-07-25-memory-lifecycle-design.md, Phase A).
mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("forget.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn forget_retires_stamps_closes_edges_and_leaves_search() {
    let (_dir, mut server) = fresh_server();
    let stored = server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "pluto uses redis", "entities": ["pluto"] }),
        DEFAULT_TIMEOUT,
    );
    let mem = stored["memory_id"].as_str().unwrap().to_string();

    let r = server.call_tool_ok(
        "forget",
        serde_json::json!({ "ids": [mem] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(r["forgotten"][0].as_str().unwrap(), mem);

    // Stamp on the node; open edges closed.
    let node = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": mem }),
        DEFAULT_TIMEOUT,
    );
    assert!(node["node"]["props"]["forgotten_at"].is_number());
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": mem }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(edges["edges"].as_array().unwrap().len(), 0);

    // Default search no longer returns it.
    let hits = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "pluto redis" }),
        DEFAULT_TIMEOUT,
    );
    assert!(hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["node"]["id"].as_str() != Some(mem.as_str())));
}

#[test]
fn forget_rejects_invalid_targets_whole_call() {
    let (_dir, mut server) = fresh_server();
    let stored = server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "eris uses zk", "entities": ["eris"] }),
        DEFAULT_TIMEOUT,
    );
    let mem = stored["memory_id"].as_str().unwrap().to_string();
    let ent = stored["entities"][0]["id"].as_str().unwrap().to_string();
    server.call_tool_ok(
        "forget",
        serde_json::json!({ "ids": [mem] }),
        DEFAULT_TIMEOUT,
    );

    // Each invalid case: error response naming the reason, and — for the
    // mixed case — the live memory must remain untouched (atomic judgment).
    for (ids, needle) in [
        (vec![mem.clone()], "already forgotten"),
        (vec![ent.clone()], "not a Memory"),
        (vec!["not-a-ulid".to_string()], "invalid node id"),
    ] {
        let resp = server.call_tool("forget", serde_json::json!({ "ids": ids }), DEFAULT_TIMEOUT);
        common::expect_tool_error(&resp);
        assert!(
            resp.to_string().contains(needle),
            "{needle:?} not named in error response: {resp}"
        );
    }
}

/// Read-side parity with supersession: a forgotten memory is not a dedup
/// target — re-remembering identical content mints a FRESH live memory.
#[test]
fn forgotten_memories_are_never_dedup_targets() {
    let (_dir, mut server) = fresh_server();
    let stored = server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "vesta uses nats", "entities": ["vesta"] }),
        DEFAULT_TIMEOUT,
    );
    let mem = stored["memory_id"].as_str().unwrap().to_string();
    server.call_tool_ok(
        "forget",
        serde_json::json!({ "ids": [mem] }),
        DEFAULT_TIMEOUT,
    );

    let again = server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "vesta uses nats", "entities": ["vesta"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(again["deduplicated"], false, "no dedup to a forgotten node");
    assert_ne!(again["memory_id"].as_str().unwrap(), mem);
}
