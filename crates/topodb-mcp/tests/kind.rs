//! Behavioral tests for the Phase B kind taxonomy (spec:
//! docs/superpowers/specs/2026-07-25-memory-lifecycle-design.md, Phase B).
mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("kind.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn remember_kind_stamps_new_memories_and_dedup_keeps_stored_kind() {
    let (_dir, mut server) = fresh_server();
    let stored = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "altair rotates certs monthly",
            "entities": ["altair"],
            "kind": "procedural"
        }),
        DEFAULT_TIMEOUT,
    );
    let mem = stored["memory_id"].as_str().unwrap().to_string();
    let node = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": mem }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(node["node"]["props"]["kind"].as_str(), Some("procedural"));

    // Dedup ignores kind; the stored kind wins.
    let again = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "altair rotates certs monthly",
            "entities": ["altair"],
            "kind": "episodic"
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(again["deduplicated"], true);
    assert_eq!(again["memory_id"].as_str().unwrap(), mem);
    let node = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": mem }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        node["node"]["props"]["kind"].as_str(),
        Some("procedural"),
        "the dedup hit's stored kind must win"
    );
}

#[test]
fn search_memories_kinds_filters_with_absent_as_semantic() {
    let (_dir, mut server) = fresh_server();
    let proc_mem = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "rigel deploys via canary",
            "entities": ["rigel"],
            "kind": "procedural"
        }),
        DEFAULT_TIMEOUT,
    )["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    let sem_mem = server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "rigel uses etcd", "entities": ["rigel"] }),
        DEFAULT_TIMEOUT,
    )["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let ids = |hits: &serde_json::Value| -> Vec<String> {
        hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["node"]["id"].as_str().map(String::from))
            .collect()
    };

    let procedural = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "rigel", "kinds": ["procedural"] }),
        DEFAULT_TIMEOUT,
    );
    assert!(ids(&procedural).contains(&proc_mem));
    assert!(!ids(&procedural).contains(&sem_mem));

    // Absent kind matches "semantic" — and so do kind-less Entity hits.
    let semantic = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "rigel", "kinds": ["semantic"] }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        ids(&semantic).contains(&sem_mem),
        "absent kind must match semantic"
    );
    assert!(!ids(&semantic).contains(&proc_mem));

    // Multi-value admits both memories.
    let both = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "rigel", "kinds": ["procedural", "semantic"] }),
        DEFAULT_TIMEOUT,
    );
    assert!(ids(&both).contains(&proc_mem) && ids(&both).contains(&sem_mem));
}

#[test]
fn kind_params_reject_bad_values_and_empty_kinds() {
    let (_dir, mut server) = fresh_server();
    for (tool, params, needle) in [
        (
            "remember",
            serde_json::json!({ "content": "x y", "entities": ["e"], "kind": "factual" }),
            "episodic",
        ),
        (
            "remember",
            serde_json::json!({ "content": "x y", "entities": ["e"],
                                "props": { "kind": "episodic" } }),
            "kind parameter",
        ),
        (
            "search_memories",
            serde_json::json!({ "query": "x", "kinds": ["Episodic"] }),
            "episodic",
        ),
        (
            "search_memories",
            serde_json::json!({ "query": "x", "kinds": [] }),
            "kinds",
        ),
    ] {
        let resp = server.call_tool(tool, params, DEFAULT_TIMEOUT);
        common::expect_tool_error(&resp);
        assert!(
            resp.to_string().contains(needle),
            "{needle:?} not named in {tool} error: {resp}"
        );
    }
}
