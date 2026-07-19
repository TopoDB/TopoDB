//! Behavioral tests for search_memories' F7 tuning params (spec:
//! docs/superpowers/specs/2026-07-19-recall-polish-design.md).
mod common;
use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("tuning.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn plumbing_nodes_never_surface_by_default() {
    let (_dir, mut server) = fresh_server();
    // A memory, an entity, an alias and a synonym all sharing a token.
    let mem = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the zephyr subsystem handles wind" }),
        DEFAULT_TIMEOUT,
    );
    let ent = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "Zephyr" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "add_alias",
        serde_json::json!({ "entity_id": ent["id"], "alias": "zephyr breeze" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "add_synonym",
        serde_json::json!({ "term": "zephyr", "expansion": "draft" }),
        DEFAULT_TIMEOUT,
    );

    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "zephyr", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let labels: Vec<String> = res["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["label"].as_str().unwrap().to_string())
        .collect();
    assert!(!labels.is_empty(), "the memory/entity must match: {res}");
    assert!(
        labels.iter().all(|l| l == "Memory" || l == "Entity"),
        "no plumbing labels in default results: {labels:?}"
    );
    let _ = mem;
}

#[test]
fn labels_override_narrows_and_empty_rejects() {
    let (_dir, mut server) = fresh_server();
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "quartz crystals resonate" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "Quartz" }),
        DEFAULT_TIMEOUT,
    );

    let narrowed = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "quartz", "k": 10, "labels": ["Memory"] }),
        DEFAULT_TIMEOUT,
    );
    let labels: Vec<&str> = narrowed["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["label"].as_str().unwrap())
        .collect();
    assert!(labels.iter().all(|l| *l == "Memory"), "{labels:?}");

    let resp = server.call_tool(
        "search_memories",
        serde_json::json!({ "query": "quartz", "k": 10, "labels": [] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn weight_bounds_reject_and_rebalance_plumbs_through() {
    let (_dir, mut server) = fresh_server();
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "basalt columns form hexagons" }),
        DEFAULT_TIMEOUT,
    );

    for bad in [
        serde_json::json!({ "query": "basalt", "k": 5, "text_weight": -1.0 }),
        serde_json::json!({ "query": "basalt", "k": 5, "graph_weight": 11.0 }),
        serde_json::json!({ "query": "basalt", "k": 5, "access_weight": 2.0 }),
        serde_json::json!({ "query": "basalt", "k": 5,
            "text_weight": 0.0, "vector_weight": 0.0, "graph_weight": 0.0 }),
    ] {
        let resp = server.call_tool("search_memories", bad.clone(), DEFAULT_TIMEOUT);
        expect_tool_error(&resp);
    }

    // Rebalance plumbs: text-only weights still find the memory (this
    // asserts the params REACH the engine, not a ranking subtlety).
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "basalt", "k": 5,
            "text_weight": 2.0, "vector_weight": 0.0, "graph_weight": 0.0 }),
        DEFAULT_TIMEOUT,
    );
    assert!(!res["hits"].as_array().unwrap().is_empty(), "{res}");
}

#[test]
fn access_weight_plumbs_through() {
    let (_dir, mut server) = fresh_server();
    let mem = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "gneiss banding under pressure" }),
        DEFAULT_TIMEOUT,
    );
    // Bump the memory's counter via reads.
    for _ in 0..5 {
        server.call_tool_ok(
            "get_node",
            serde_json::json!({ "id": mem["id"] }),
            DEFAULT_TIMEOUT,
        );
    }
    // With the boost on, the memory must still be found and ranked (single-
    // node corpus: this asserts plumbing + no crash on the counter-read
    // path, not ranking).
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "gneiss", "k": 5, "access_weight": 1.0 }),
        DEFAULT_TIMEOUT,
    );
    let hits = res["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{res}");
    assert_eq!(hits[0]["node"]["id"], mem["id"]);
}
