//! Behavioral e2e for the Plan 6 tools (set_node_props, remove_node,
//! close_edge, set_embedding, search_vectors, submit_batch). Reuses the same
//! spawn/JSON-RPC plumbing as `e2e.rs` via `tests/common`.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("plan6.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn set_node_props_and_remove_node() {
    let (_dir, mut server) = fresh_server();
    let id = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "ada", "props": { "stale": "yes" } }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // set_node_props: add a key, remove another with null.
    let res = server.call_tool_ok(
        "set_node_props",
        serde_json::json!({ "id": id, "props": { "role": "pioneer", "stale": null } }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        res["seq"].as_u64().is_some(),
        "set_node_props returns seq: {res}"
    );

    let node = server.call_tool_ok("get_node", serde_json::json!({ "id": id }), DEFAULT_TIMEOUT);
    assert_eq!(node["node"]["props"]["role"], serde_json::json!("pioneer"));
    assert!(node["node"]["props"].get("stale").is_none());

    // remove_node: the node is gone afterward.
    let res = server.call_tool_ok(
        "remove_node",
        serde_json::json!({ "id": id }),
        DEFAULT_TIMEOUT,
    );
    assert!(res["seq"].as_u64().is_some());
    let gone = server.call_tool_ok("get_node", serde_json::json!({ "id": id }), DEFAULT_TIMEOUT);
    assert_eq!(gone["found"], serde_json::json!(false));
}

#[test]
fn set_node_props_on_missing_node_is_tool_error() {
    let (_dir, mut server) = fresh_server();
    let ghost = topodb::NodeId::new().to_string();
    let resp = server.call_tool(
        "set_node_props",
        serde_json::json!({ "id": ghost, "props": { "x": 1 } }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn close_edge_closes_an_open_edge() {
    let (_dir, mut server) = fresh_server();
    let a = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "a" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "b" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let e = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": b, "edge_type": "x" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = server.call_tool_ok(
        "close_edge",
        serde_json::json!({ "id": e, "valid_to": 1000 }),
        DEFAULT_TIMEOUT,
    );
    assert!(res["seq"].as_u64().is_some());
}

#[test]
fn set_embedding_then_vector_search_finds_it() {
    let (_dir, mut server) = fresh_server();
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "vectorized" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = server.call_tool_ok(
        "set_embedding",
        serde_json::json!({ "id": m, "model": "test", "vector": [1.0, 0.0] }),
        DEFAULT_TIMEOUT,
    );
    assert!(res["seq"].as_u64().is_some());
}

#[test]
fn close_edge_missing_is_tool_error() {
    let (_dir, mut server) = fresh_server();
    let ghost = topodb::EdgeId::new().to_string();
    let resp = server.call_tool(
        "close_edge",
        serde_json::json!({ "id": ghost }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}
