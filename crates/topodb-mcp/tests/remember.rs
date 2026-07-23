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

    // And the edge actually persisted, one edge into the pre-existing node.
    let memory_id = res["memory_id"].as_str().unwrap();
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": memory_id }),
        DEFAULT_TIMEOUT,
    );
    let arr = edges["edges"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "exactly one persisted edge: {edges}");
    assert_eq!(arr[0]["to"].as_str().unwrap(), drew);
}

#[test]
fn remember_collapses_alias_and_canonical_to_one_entity() {
    let (_dir, mut server) = fresh_server();

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

    // Canonical name AND its alias in one call: both resolve to the same
    // node — one row, one edge, not two.
    let res = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "alias and canonical collapse",
            "entities": ["Drew Powell", "Drew"],
        }),
        DEFAULT_TIMEOUT,
    );
    let ents = res["entities"].as_array().unwrap();
    assert_eq!(ents.len(), 1, "same node = one row: {res}");
    assert_eq!(ents[0]["id"].as_str().unwrap(), drew);
    assert_eq!(ents[0]["name"], "Drew Powell", "first spelling wins");
    assert_eq!(res["edge_ids"].as_array().unwrap().len(), 1);

    let memory_id = res["memory_id"].as_str().unwrap();
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": memory_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        edges["edges"].as_array().unwrap().len(),
        1,
        "one edge, not a duplicate pair"
    );
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
        // Invalid scope: not "shared" and not a ULID.
        serde_json::json!({ "content": "c", "entities": ["X"], "scope": "not-a-ulid" }),
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

/// Externally-tagged Op JSON: the variant is the single top-level key.
fn op_kind(op: &serde_json::Value) -> String {
    op.as_object().unwrap().keys().next().unwrap().clone()
}

/// Extract error message from tool response. Handles both direct error objects
/// and error messages wrapped in tool result content blocks.
fn extract_error_message(resp: &serde_json::Value) -> &str {
    if let Some(err) = resp.get("error") {
        err.get("message").and_then(|m| m.as_str()).unwrap_or("")
    } else if let Some(result) = resp.get("result") {
        result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
    } else {
        ""
    }
}

#[test]
fn remember_lands_as_one_contiguous_op_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("remember-atomic.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(
        &db_path,
        &["--scope", scope.as_str(), "--allow-unscoped-changes"],
    );
    server.initialize(DEFAULT_TIMEOUT);

    let seq_before = current_seq(&mut server);
    server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "atomic batch",
            "entities": ["A", "B"],
        }),
        DEFAULT_TIMEOUT,
    );

    let changes = server.call_tool_ok(
        "get_changes",
        serde_json::json!({ "since_seq": seq_before + 1 }),
        DEFAULT_TIMEOUT,
    );
    let ops = changes["ops"].as_array().unwrap();

    // Contiguous: one batch, no gaps — nothing else interleaved.
    let seqs: Vec<u64> = ops.iter().map(|o| o["seq"].as_u64().unwrap()).collect();
    let expected: Vec<u64> = (seq_before + 1..=seq_before + seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "seqs must be contiguous: {ops:#?}");

    // Exactly the call's writes: 3 CreateNode (1 Memory + 2 Entity) and
    // 2 CreateEdge. SetEmbedding ops are host-dependent (present only when
    // an ONNX runtime is available), so they're counted separately and
    // otherwise ignored.
    let kinds: Vec<String> = ops.iter().map(|o| op_kind(&o["op"])).collect();
    let creates = kinds.iter().filter(|k| *k == "CreateNode").count();
    let edges = kinds.iter().filter(|k| *k == "CreateEdge").count();
    let embeds = kinds.iter().filter(|k| *k == "SetEmbedding").count();
    assert_eq!(creates, 3, "1 memory + 2 entities: {kinds:?}");
    assert_eq!(edges, 2, "one link per entity: {kinds:?}");
    assert_eq!(
        kinds.len(),
        creates + edges + embeds,
        "no unexpected op kinds: {kinds:?}"
    );
}

#[test]
fn validation_errors_precede_scope_resolution() {
    let (_dir, mut server) = fresh_server();

    // A call with BOTH an invalid scope AND empty entities must return the
    // entities error (input validation), not the scope error (scope resolution).
    let resp = server.call_tool(
        "remember",
        serde_json::json!({
            "content": "c",
            "entities": [],
            "scope": "not-a-ulid",
        }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
    // Error structure: either JSON-RPC `error` (protocol error) or `result.isError`.
    let error_msg = extract_error_message(&resp);
    assert!(
        error_msg.contains("entities must contain at least one name"),
        "expected entities error, got: {error_msg}, full response: {resp}"
    );
}

#[test]
fn remember_rejects_reserved_props_keys() {
    let (_dir, mut server) = fresh_server();

    // remember with content_hash in props must be rejected.
    let resp = server.call_tool(
        "remember",
        serde_json::json!({
            "content": "fact A",
            "entities": ["X"],
            "props": { "content_hash": "x" }
        }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
    let error_msg = extract_error_message(&resp);
    assert!(
        error_msg.contains("content_hash")
            && error_msg.contains("maintained by the engine write path"),
        "expected content_hash rejection, got: {error_msg}"
    );

    // remember with superseded_at in props must also be rejected.
    let resp = server.call_tool(
        "remember",
        serde_json::json!({
            "content": "fact B",
            "entities": ["X"],
            "props": { "superseded_at": 12345 }
        }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
    let error_msg = extract_error_message(&resp);
    assert!(
        error_msg.contains("superseded_at")
            && error_msg.contains("maintained by the engine write path"),
        "expected superseded_at rejection, got: {error_msg}"
    );

    // create_memory with content_hash in props must be rejected.
    let resp = server.call_tool(
        "create_memory",
        serde_json::json!({
            "content": "fact C",
            "props": { "content_hash": "x" }
        }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
    let error_msg = extract_error_message(&resp);
    assert!(
        error_msg.contains("content_hash")
            && error_msg.contains("maintained by the engine write path"),
        "expected content_hash rejection in create_memory, got: {error_msg}"
    );

    // create_memory with superseded_at in props must also be rejected.
    let resp = server.call_tool(
        "create_memory",
        serde_json::json!({
            "content": "fact D",
            "props": { "superseded_at": 12345 }
        }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
    let error_msg = extract_error_message(&resp);
    assert!(
        error_msg.contains("superseded_at")
            && error_msg.contains("maintained by the engine write path"),
        "expected superseded_at rejection in create_memory, got: {error_msg}"
    );
}

#[test]
fn create_memory_rejects_reserved_keys_even_on_dedup() {
    let (_dir, mut server) = fresh_server();

    // First: create memory without props
    let res1 = server.call_tool_ok(
        "create_memory",
        serde_json::json!({
            "content": "x",
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(!res1["deduplicated"].as_bool().unwrap());

    // Second: re-send the SAME content WITH reserved-key props → must be rejected, not deduplicated
    for (content, reserved_key, reserved_val) in [
        ("x", "content_hash", serde_json::json!("boom")),
        ("x", "superseded_at", serde_json::json!(1)),
    ] {
        let resp = server.call_tool(
            "create_memory",
            serde_json::json!({
                "content": content,
                "props": { reserved_key: reserved_val }
            }),
            DEFAULT_TIMEOUT,
        );
        common::expect_tool_error(&resp);
        let error_msg = extract_error_message(&resp);
        assert!(
            error_msg.contains(reserved_key)
                && error_msg.contains("maintained by the engine write path"),
            "expected {reserved_key} rejection even on dedup, got: {error_msg}"
        );
    }
}

#[test]
fn remember_rejects_reserved_keys_even_on_dedup() {
    let (_dir, mut server) = fresh_server();

    // First: remember content with entity
    let res1 = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "y",
            "entities": ["E"],
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(!res1["deduplicated"].as_bool().unwrap());

    // Second: re-send the SAME content WITH reserved-key props → must be rejected, not deduplicated
    for (content, reserved_key, reserved_val) in [
        ("y", "content_hash", serde_json::json!("boom")),
        ("y", "superseded_at", serde_json::json!(1)),
    ] {
        let resp = server.call_tool(
            "remember",
            serde_json::json!({
                "content": content,
                "entities": ["E"],
                "props": { reserved_key: reserved_val }
            }),
            DEFAULT_TIMEOUT,
        );
        common::expect_tool_error(&resp);
        let error_msg = extract_error_message(&resp);
        assert!(
            error_msg.contains(reserved_key)
                && error_msg.contains("maintained by the engine write path"),
            "expected {reserved_key} rejection even on dedup, got: {error_msg}"
        );
    }
}

#[test]
fn re_remember_of_superseded_content_mints_fresh_memory() {
    let (_dir, mut server) = fresh_server();

    // Store fact A.
    let res_a = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "fact A content",
            "entities": ["Entity1"],
        }),
        DEFAULT_TIMEOUT,
    );
    let memory_a = res_a["memory_id"].as_str().unwrap().to_string();

    // Store fact B, superseding fact A.
    let res_b = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "fact B content",
            "entities": ["Entity1"],
            "supersedes": [memory_a.clone()]
        }),
        DEFAULT_TIMEOUT,
    );
    let memory_b = res_b["memory_id"].as_str().unwrap().to_string();

    // Re-remember fact A's content: should NOT dedup to the superseded node,
    // but instead return a fresh memory with deduplicated: false.
    let res_re_a = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "fact A content",
            "entities": ["Entity1"],
        }),
        DEFAULT_TIMEOUT,
    );
    let memory_re_a = res_re_a["memory_id"].as_str().unwrap().to_string();

    assert_eq!(
        res_re_a["deduplicated"], false,
        "re-remember of superseded content must not dedup"
    );
    assert_ne!(
        memory_re_a, memory_a,
        "re-remember must return a fresh memory_id, not the superseded one"
    );
    assert_ne!(
        memory_re_a, memory_b,
        "re-remember must not return the other memory either"
    );
}

#[test]
fn create_memory_with_superseded_content_mints_fresh_memory() {
    let (_dir, mut server) = fresh_server();

    // Store fact A via remember.
    let res_a = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "fact A content",
            "entities": ["Entity1"],
        }),
        DEFAULT_TIMEOUT,
    );
    let memory_a = res_a["memory_id"].as_str().unwrap().to_string();

    // Store fact B via remember, superseding fact A.
    server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "fact B content",
            "entities": ["Entity1"],
            "supersedes": [memory_a.clone()]
        }),
        DEFAULT_TIMEOUT,
    );

    // create_memory with fact A's content: should NOT dedup to the superseded
    // node, but instead return a fresh memory with deduplicated: false.
    let res_create_a = server.call_tool_ok(
        "create_memory",
        serde_json::json!({
            "content": "fact A content",
        }),
        DEFAULT_TIMEOUT,
    );
    let memory_create_a = res_create_a["id"].as_str().unwrap().to_string();

    assert_eq!(
        res_create_a["deduplicated"], false,
        "create_memory with superseded content must not dedup"
    );
    assert_ne!(
        memory_create_a, memory_a,
        "create_memory must return a fresh memory_id, not the superseded one"
    );
}
