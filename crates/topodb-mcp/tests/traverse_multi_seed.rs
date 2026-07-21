//! Multi-seed traverse: an agent that just recalled several memories can walk
//! the graph around ALL of them in a single call, instead of one traverse per
//! anchor (each MCP call is a model turn). `seed_id` (single) still works —
//! backward compatible — and providing neither is a clean error.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

fn entity_id(server: &mut Server, name: &str) -> String {
    let r = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": name }),
        DEFAULT_TIMEOUT,
    );
    r["nodes"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("entity '{name}' should resolve to a node: {r:#?}"))
        .to_string()
}

#[test]
fn traverse_accepts_multiple_seeds_and_still_accepts_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    // Two facts, each linked to its own entity.
    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "ALPHA-FACT about the alpha thing", "entities": ["Alpha"] }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "BRAVO-FACT about the bravo thing", "entities": ["Bravo"] }),
        DEFAULT_TIMEOUT,
    );
    let alpha = entity_id(&mut server, "Alpha");
    let bravo = entity_id(&mut server, "Bravo");

    // Multi-seed: ONE traverse reaches BOTH anchors' memories.
    let multi = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_ids": [alpha, bravo], "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let multi_blob = multi["subgraph"].to_string();
    assert!(
        multi_blob.contains("ALPHA-FACT") && multi_blob.contains("BRAVO-FACT"),
        "multi-seed traverse must reach both anchors' memories in one call: {}",
        multi["subgraph"]
    );

    // Single `seed_id` still works and reaches only its own anchor.
    let single = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": alpha, "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let single_blob = single["subgraph"].to_string();
    assert!(
        single_blob.contains("ALPHA-FACT") && !single_blob.contains("BRAVO-FACT"),
        "single seed_id must still work and reach only its anchor: {}",
        single["subgraph"]
    );
}

#[test]
fn traverse_with_no_seed_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    // Neither seed_id nor seed_ids: a clear invalid-params error, not a panic.
    let resp = server.call_tool(
        "traverse",
        serde_json::json!({ "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // An empty seed_ids is likewise rejected (an empty anchor set walks nothing).
    let resp2 = server.call_tool(
        "traverse",
        serde_json::json!({ "seed_ids": [], "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp2);
}
