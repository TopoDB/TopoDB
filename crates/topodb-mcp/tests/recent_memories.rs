//! Behavioral tests for recent_memories (spec:
//! docs/superpowers/specs/2026-07-19-plugin-auto-capture-design.md).
mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("recent.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn returns_newest_memories_first_capped_at_k() {
    let (_dir, mut server) = fresh_server();
    let mut ids = Vec::new();
    for content in ["first", "second", "third"] {
        ids.push(
            server.call_tool_ok(
                "create_memory",
                serde_json::json!({ "content": content }),
                DEFAULT_TIMEOUT,
            )["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    // Entities must NOT appear — Memory label only.
    server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "Distractor" }),
        DEFAULT_TIMEOUT,
    );

    let res = server.call_tool_ok(
        "recent_memories",
        serde_json::json!({ "k": 2 }),
        DEFAULT_TIMEOUT,
    );
    let mems = res["memories"].as_array().unwrap();
    assert_eq!(mems.len(), 2, "k caps the result: {res}");
    // Newest first: the third and second created (ULIDs mint in order).
    assert_eq!(mems[0]["id"].as_str().unwrap(), ids[2]);
    assert_eq!(mems[1]["id"].as_str().unwrap(), ids[1]);
    assert!(mems.iter().all(|m| m["label"] == "Memory"));
    assert_eq!(mems[0]["props"]["content"], "third");
}

#[test]
fn default_k_and_bounds() {
    let (_dir, mut server) = fresh_server();
    // Empty db: empty list, not an error.
    let res = server.call_tool_ok("recent_memories", serde_json::json!({}), DEFAULT_TIMEOUT);
    assert_eq!(res["memories"].as_array().unwrap().len(), 0);
    // k out of bounds is rejected.
    let resp = server.call_tool(
        "recent_memories",
        serde_json::json!({ "k": 0 }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
    let resp = server.call_tool(
        "recent_memories",
        serde_json::json!({ "k": 101 }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
}
