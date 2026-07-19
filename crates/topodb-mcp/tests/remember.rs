//! Behavioral tests for the composed `remember` tool (spec:
//! docs/superpowers/specs/2026-07-18-remember-verb-design.md).
mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("remember.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn remember_stores_memory_entities_and_links_in_one_call() {
    let (_dir, mut server) = fresh_server();

    let res = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "Drew wants HNSW behind a feature flag",
            "entities": ["Drew Powell", "TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );

    // Result shape: input-ordered entities, index-aligned edge_ids.
    let memory_id = res["memory_id"].as_str().unwrap().to_string();
    let ents = res["entities"].as_array().unwrap();
    assert_eq!(ents.len(), 2, "two entities in: {res}");
    assert_eq!(ents[0]["name"], "Drew Powell");
    assert_eq!(ents[1]["name"], "TopoDB");
    assert_eq!(ents[0]["created"], true);
    assert_eq!(ents[1]["created"], true);
    let edge_ids = res["edge_ids"].as_array().unwrap();
    assert_eq!(edge_ids.len(), 2);

    // The memory node exists with label Memory and the content prop.
    let node = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": memory_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(node["found"], true);
    assert_eq!(node["node"]["label"], "Memory");
    assert_eq!(
        node["node"]["props"]["content"],
        "Drew wants HNSW behind a feature flag"
    );

    // Both links exist, open, default type "about", memory -> entity.
    let entity_ids: Vec<String> = ents
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": memory_id }),
        DEFAULT_TIMEOUT,
    );
    let arr = edges["edges"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "two about-edges out of the memory: {edges}");
    for e in arr {
        assert_eq!(e["type"], "about");
        assert_eq!(e["valid_to"], serde_json::Value::Null);
        assert!(entity_ids.contains(&e["to"].as_str().unwrap().to_string()));
    }

    // And each entity node really exists.
    for id in &entity_ids {
        let n = server.call_tool_ok("get_node", serde_json::json!({ "id": id }), DEFAULT_TIMEOUT);
        assert_eq!(n["found"], true);
        assert_eq!(n["node"]["label"], "Entity");
    }
}

#[test]
fn remember_reuses_existing_entities_and_aliases() {
    let (_dir, mut server) = fresh_server();

    // Pre-existing entity + an alias for it.
    let drew = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "Drew Powell" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.call_tool_ok(
        "add_alias",
        serde_json::json!({ "entity_id": drew, "alias": "Drew" }),
        DEFAULT_TIMEOUT,
    );

    // Case-variant of the ALIAS must resolve to the canonical entity.
    let res = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "prefers spec-first development",
            "entities": ["drew"],
        }),
        DEFAULT_TIMEOUT,
    );
    let ents = res["entities"].as_array().unwrap();
    assert_eq!(ents.len(), 1);
    assert_eq!(ents[0]["id"].as_str().unwrap(), drew);
    assert_eq!(ents[0]["created"], false);
}

#[test]
fn remember_collapses_repeated_names_in_one_call() {
    let (_dir, mut server) = fresh_server();

    let res = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "dedup within a single call",
            "entities": ["Drew Powell", " drew   powell "],
        }),
        DEFAULT_TIMEOUT,
    );
    let ents = res["entities"].as_array().unwrap();
    assert_eq!(ents.len(), 1, "case/whitespace variants collapse: {res}");
    assert_eq!(ents[0]["name"], "Drew Powell", "first spelling wins");
    assert_eq!(res["edge_ids"].as_array().unwrap().len(), 1);

    // Exactly one edge out of the memory.
    let memory_id = res["memory_id"].as_str().unwrap();
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": memory_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(edges["edges"].as_array().unwrap().len(), 1);
}

#[test]
fn remember_normalizes_custom_edge_types() {
    let (_dir, mut server) = fresh_server();
    let res = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "custom edge type",
            "entities": ["Spec"],
            "edge_type": "Mentioned In",
        }),
        DEFAULT_TIMEOUT,
    );
    let memory_id = res["memory_id"].as_str().unwrap();
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": memory_id }),
        DEFAULT_TIMEOUT,
    );
    let arr = edges["edges"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "mentioned_in", "link-style normalization");
}

/// db_info's current_seq — the op-log high-water mark. If a rejected call
/// wrote anything at all, this moves.
fn current_seq(server: &mut Server) -> u64 {
    server.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT)["current_seq"]
        .as_u64()
        .unwrap()
}

#[test]
fn rejected_remember_calls_write_nothing() {
    let (_dir, mut server) = fresh_server();
    // One good write so the baseline seq is nonzero.
    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "baseline", "entities": ["X"] }),
        DEFAULT_TIMEOUT,
    );
    let seq_before = current_seq(&mut server);

    for bad in [
        // Empty entities: minItems 1 (schema) + runtime check.
        serde_json::json!({ "content": "c", "entities": [] }),
        // Blank entity name.
        serde_json::json!({ "content": "c", "entities": ["   "] }),
        // Invalid edge type (normalize_edge_type rejects empty).
        serde_json::json!({ "content": "c", "entities": ["X"], "edge_type": "" }),
        // Unknown field: deny_unknown_fields.
        serde_json::json!({ "content": "c", "entities": ["X"], "contnet": "typo" }),
        // props colliding with the reserved content key.
        serde_json::json!({ "content": "c", "entities": ["X"], "props": { "content": "clash" } }),
    ] {
        let resp = server.call_tool("remember", bad.clone(), DEFAULT_TIMEOUT);
        common::expect_tool_error(&resp);
        assert_eq!(
            current_seq(&mut server),
            seq_before,
            "rejected call must write nothing, but seq moved for: {bad}"
        );
    }
}
