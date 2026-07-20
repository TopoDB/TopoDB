//! suggest_links tool surface: enriched common_neighbors evidence
//! ({id, label, name}), similarity field, and min_similarity validation.
//! Spec: 2026-07-20 suggest_links polish design.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};
use serde_json::json;

fn id_of(v: &serde_json::Value) -> String {
    v.get("id")
        .and_then(|x| x.as_str())
        .unwrap_or_else(|| panic!("expected structured id in {v:#?}"))
        .to_string()
}

#[test]
fn evidence_objects_similarity_null_and_floor_validation() {
    let dir = tempfile::tempdir().unwrap();
    let scope = topodb::ScopeId::new().to_string();
    let scope_args = ["--scope", scope.as_str(), "--embeddings", "off"];
    let mut server = Server::spawn(&dir.path().join("t.redb"), &scope_args);
    server.initialize(DEFAULT_TIMEOUT);

    // a—ent, b—ent (a and b share ent; no a—b edge) — b is a's suggestion
    // with ent as evidence. A long-content memory checks name truncation.
    let ent = id_of(&server.call_tool_ok(
        "create_entity",
        json!({ "name": "Search Service" }),
        DEFAULT_TIMEOUT,
    ));
    let a = id_of(&server.call_tool_ok(
        "create_memory",
        json!({ "content": "memory a about the search service" }),
        DEFAULT_TIMEOUT,
    ));
    let b = id_of(&server.call_tool_ok(
        "create_memory",
        json!({ "content": "memory b about the search service" }),
        DEFAULT_TIMEOUT,
    ));
    // A second shared neighbor with LONG content pins the 80-char + '…'
    // truncation rule for Memory-labeled evidence.
    let hub = id_of(&server.call_tool_ok(
        "create_memory",
        json!({ "content": "x".repeat(100) }),
        DEFAULT_TIMEOUT,
    ));
    for from in [&a, &b] {
        for to in [&ent, &hub] {
            server.call_tool_ok(
                "link",
                json!({ "from_id": from, "to_id": to, "edge_type": "about" }),
                DEFAULT_TIMEOUT,
            );
        }
    }

    let out = server.call_tool_ok(
        "suggest_links",
        json!({ "node_id": a, "k": 5 }),
        DEFAULT_TIMEOUT,
    );
    let suggestions = out
        .get("suggestions")
        .and_then(|s| s.as_array())
        .expect("suggestions array");
    let top = &suggestions[0];
    assert_eq!(
        top.get("node")
            .and_then(|n| n.get("id"))
            .and_then(|v| v.as_str()),
        Some(b.as_str()),
        "b (shared entity neighbor) must be the top suggestion: {top:#?}"
    );
    assert!(
        top.get("similarity").is_some_and(|s| s.is_null()),
        "structural-only suggestion must carry similarity: null: {top:#?}"
    );
    let neighbors = top
        .get("common_neighbors")
        .and_then(|c| c.as_array())
        .expect("common_neighbors array");
    assert_eq!(
        neighbors.len(),
        2,
        "two shared neighbors (entity + hub memory)"
    );
    let by_id = |want: &str| {
        neighbors
            .iter()
            .find(|n| n.get("id").and_then(|v| v.as_str()) == Some(want))
            .unwrap_or_else(|| panic!("neighbor {want} missing: {neighbors:#?}"))
    };
    let ne = by_id(&ent);
    assert_eq!(ne.get("label").and_then(|v| v.as_str()), Some("Entity"));
    assert_eq!(
        ne.get("name").and_then(|v| v.as_str()),
        Some("Search Service"),
        "entity evidence renders its name prop: {ne:#?}"
    );
    let nh = by_id(&hub);
    assert_eq!(nh.get("label").and_then(|v| v.as_str()), Some("Memory"));
    let hub_name = nh.get("name").and_then(|v| v.as_str()).expect("hub name");
    assert_eq!(
        hub_name.chars().count(),
        81,
        "100-char content truncates to 80 chars + '…': {hub_name:?}"
    );
    assert!(hub_name.ends_with('…'));

    // Floor knob: out-of-schema-range value still reaches the engine and
    // comes back invalid_params (schema-advertised, engine-enforced).
    let bad = server.call_tool(
        "suggest_links",
        json!({ "node_id": a, "k": 5, "min_similarity": 1.5 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&bad);
}
