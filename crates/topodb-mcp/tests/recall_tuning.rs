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

/// The alias→entity edge must let a query that matches ONLY the alias's name
/// still surface the canonical entity — pulled in via `graph_boost`'s 1-hop
/// traversal from the alias node (a preliminary-fusion seed) — while the
/// Alias node itself stays hidden behind the default `["Memory","Entity"]`
/// label filter (Task 2 / finding 6). If the alias→entity edge does not
/// survive that 1-hop pull in practice, this test is expected to fail
/// honestly rather than being loosened to force a pass.
#[test]
fn alias_query_surfaces_entity_via_graph_seed_not_alias() {
    let (_dir, mut server) = fresh_server();
    let ent = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "Nimbus" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "add_alias",
        serde_json::json!({ "entity_id": ent["id"], "alias": "nimbus cloudform" }),
        DEFAULT_TIMEOUT,
    );

    // "cloudform" matches only the alias's name — not the entity's own name
    // ("Nimbus") and nothing else in the corpus.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "cloudform", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let hits = res["hits"].as_array().unwrap();
    let ids: Vec<&str> = hits
        .iter()
        .map(|h| h["node"]["id"].as_str().unwrap())
        .collect();
    let labels: Vec<&str> = hits
        .iter()
        .map(|h| h["node"]["label"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&ent["id"].as_str().unwrap()),
        "the canonical entity must surface via the alias's graph adjacency: {res}"
    );
    assert!(
        !labels.contains(&"Alias"),
        "the Alias plumbing node must not surface in default results: {res}"
    );
}

#[test]
fn entity_ranks_below_memory_by_default() {
    let (_dir, mut server) = fresh_server();
    // A linked memory about an entity with the same name: entity "login" linked
    // from a memory mentioning "login". This covers the graph-leg contribution
    // that the field failure tested.
    let _mem = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the login flow breaks on big cookies",
            "entities": ["login"]
        }),
        DEFAULT_TIMEOUT,
    );

    // Search for "login trouble" — matches the memory content directly and
    // the entity name. With the default label_weights (Entity: 0.5), the
    // Entity hit should rank BELOW the Memory hit.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "login trouble", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let hits = res["hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "the search must find results: {res}");

    let mut memory_idx = None;
    let mut entity_idx = None;
    for (i, hit) in hits.iter().enumerate() {
        let label = hit["node"]["label"].as_str().unwrap();
        if label == "Memory" && memory_idx.is_none() {
            memory_idx = Some(i);
        }
        if label == "Entity" && entity_idx.is_none() {
            entity_idx = Some(i);
        }
    }

    assert!(memory_idx.is_some(), "the memory must be in results: {res}");
    assert!(entity_idx.is_some(), "the entity must be in results: {res}");

    let memory_idx = memory_idx.unwrap();
    let entity_idx = entity_idx.unwrap();
    assert!(
        memory_idx < entity_idx,
        "Memory at position {memory_idx} must rank ABOVE Entity at position {entity_idx} with default label_weights: {res}"
    );
}

#[test]
fn empty_label_weights_restores_old_order() {
    let (_dir, mut server) = fresh_server();
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the login flow breaks on big cookies" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "login" }),
        DEFAULT_TIMEOUT,
    );

    // Search with default label_weights (Entity: 0.5)
    let res_default = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "login trouble", "k": 10 }),
        DEFAULT_TIMEOUT,
    );

    // Search with explicit empty label_weights (no down-weighting)
    let res_empty = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "login trouble", "k": 10, "label_weights": {} }),
        DEFAULT_TIMEOUT,
    );

    // Find Entity hits in both results
    let find_entity_score = |hits: &serde_json::Value| -> Option<f32> {
        hits.as_array()
            .unwrap()
            .iter()
            .find(|h| h["node"]["label"].as_str().unwrap() == "Entity")
            .map(|h| h["score"].as_f64().unwrap() as f32)
    };

    let score_default = find_entity_score(&res_default["hits"]);
    let score_empty = find_entity_score(&res_empty["hits"]);

    assert!(
        score_default.is_some() && score_empty.is_some(),
        "Entity must appear in both searches: default={res_default}, empty={res_empty}"
    );

    let score_default = score_default.unwrap();
    let score_empty = score_empty.unwrap();

    // With Entity: 0.5, the score should be exactly half of the no-weight score
    // (within float tolerance).
    let ratio = score_default / score_empty;
    let tolerance = 0.0001;
    assert!(
        (ratio - 0.5).abs() < tolerance,
        "Entity score with default (0.5) should be ~0.5x the score with {{}}: {score_default} vs {score_empty}, ratio={ratio}"
    );
}

#[test]
fn label_weights_validation() {
    let (_dir, mut server) = fresh_server();
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "basalt columns form hexagons" }),
        DEFAULT_TIMEOUT,
    );

    // Test 1: negative weight should be rejected
    let resp = server.call_tool(
        "search_memories",
        serde_json::json!({ "query": "basalt", "k": 5, "label_weights": {"Entity": -1.0} }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // Test 2: out-of-range weight (11.0 > max 10.0)
    let resp = server.call_tool(
        "search_memories",
        serde_json::json!({ "query": "basalt", "k": 5, "label_weights": {"Entity": 11.0} }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // Test 3: empty label name
    let resp = server.call_tool(
        "search_memories",
        serde_json::json!({ "query": "basalt", "k": 5, "label_weights": {"": 0.5} }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}
