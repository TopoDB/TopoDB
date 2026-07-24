//! Content dedup for `remember` / `create_memory`: re-storing an identical
//! fact returns the existing memory instead of accumulating duplicates — the
//! memory-hygiene an agent needs when it re-processes the same context across
//! sessions. Dedup is exact (normalized) and scoped to the write scope; a
//! re-remember with a NEW entity enriches the existing memory's links.

mod common;

use common::{Server, DEFAULT_TIMEOUT};

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";
const B: &str = "01HZY0BBBBBBBBBBBBBBBBBBBB";

#[test]
fn create_memory_dedups_identical_content() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(DEFAULT_TIMEOUT);

    let first = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "The deploy target is Fly.io" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(first["deduplicated"], serde_json::json!(false));

    // Same content, even with different surrounding whitespace, dedups.
    let again = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "  The deploy target is   Fly.io " }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(again["deduplicated"], serde_json::json!(true));
    assert_eq!(
        again["id"], first["id"],
        "dedup must return the existing id"
    );

    // Different content is a new memory.
    let other = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "The deploy target is Render" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(other["deduplicated"], serde_json::json!(false));
    assert_ne!(other["id"], first["id"]);
}

#[test]
fn remember_dedups_and_enriches_links() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(DEFAULT_TIMEOUT);

    let first = s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "auth issues JWTs", "entities": ["Auth"] }),
        DEFAULT_TIMEOUT,
    );
    let mem = first["memory_id"].clone();
    assert_eq!(first["deduplicated"], serde_json::json!(false));

    // Re-remember the SAME fact about the SAME entity: pure no-op, same memory.
    let dup = s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "auth issues JWTs", "entities": ["Auth"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(dup["deduplicated"], serde_json::json!(true));
    assert_eq!(dup["memory_id"], mem, "no duplicate memory node");

    // Re-remember the SAME fact but naming a NEW entity: the existing memory is
    // reused (no duplicate) AND the new entity is linked to it.
    let enriched = s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "auth issues JWTs", "entities": ["Security"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(enriched["deduplicated"], serde_json::json!(true));
    assert_eq!(enriched["memory_id"], mem);

    // Traversing from the memory now reaches BOTH entities.
    let tr = s.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": mem, "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let blob = tr["subgraph"].to_string();
    assert!(
        blob.contains("Auth") && blob.contains("Security"),
        "the deduped memory must link both the original and the newly-remembered entity: {}",
        tr["subgraph"]
    );

    // And there is exactly ONE memory for this content — a search returns a
    // single Memory node, not two.
    let hits = s.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "auth JWTs", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let mem_hits = hits["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|h| h["node"]["label"] == "Memory" || h["label"] == "Memory")
                .count()
        })
        .unwrap_or(0);
    assert!(
        mem_hits <= 1,
        "content must dedup to a single memory node, got {mem_hits}: {hits}"
    );
}

#[test]
fn dedup_is_scoped_to_the_write_scope() {
    let dir = tempfile::tempdir().unwrap();
    // Default write scope A, but reads span A and B so we can see both.
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--read-scopes", &format!("{A},{B}")],
    );
    s.initialize(DEFAULT_TIMEOUT);

    let in_a = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "SHARED-STRING fact", "scope": A }),
        DEFAULT_TIMEOUT,
    );
    let in_b = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "SHARED-STRING fact", "scope": B }),
        DEFAULT_TIMEOUT,
    );
    // Same content, different scopes => two distinct memories (a project's
    // memory must not dedup against another project's).
    assert_eq!(in_b["deduplicated"], serde_json::json!(false));
    assert_ne!(in_a["id"], in_b["id"]);
}

#[test]
fn find_duplicate_memories_text_fallback_with_embeddings_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Seed two overlapping memories.
    let overlap1 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the login flow breaks when the session cookie exceeds four kilobytes" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let overlap2 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the login flow breaks when the session cookie exceeds the size limit" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Seed one disjoint memory.
    let _disjoint = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the office plants are watered on Tuesdays" }),
        DEFAULT_TIMEOUT,
    );

    // Call find_duplicate_memories with embeddings off.
    let result = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({ "min_similarity": 0.6 }),
        DEFAULT_TIMEOUT,
    );

    // Check method field.
    assert_eq!(
        result["method"].as_str().unwrap(),
        "text",
        "method should be 'text' when embeddings are off: {result:#?}"
    );

    // Check that we scanned at least 3 memories.
    assert!(
        result["scanned"].as_u64().unwrap() >= 3,
        "should scan all 3 memories: {result:#?}"
    );

    // Check that we found exactly the overlapping pair.
    let pairs = result["pairs"].as_array().unwrap();
    assert_eq!(
        pairs.len(),
        1,
        "should find exactly 1 overlapping pair: {result:#?}"
    );

    let pair = &pairs[0];
    let ids = pair["ids"].as_array().unwrap();
    let pair_ids = (
        ids[0].as_str().unwrap().to_string(),
        ids[1].as_str().unwrap().to_string(),
    );
    let overlap_pair = if overlap1 <= overlap2 {
        (overlap1.clone(), overlap2.clone())
    } else {
        (overlap2.clone(), overlap1.clone())
    };
    assert_eq!(
        (pair_ids.0.clone(), pair_ids.1.clone()),
        overlap_pair,
        "should find the overlapping pair: {result:#?}"
    );
}

#[test]
fn find_duplicate_memories_exact_boundary_at_0_75_containment() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Seed two memories with token containment exactly 0.75:
    // "alpha beta gamma delta" → {alpha, beta, gamma, delta} = 4 tokens
    // "alpha beta gamma epsilon" → {alpha, beta, gamma, epsilon} = 4 tokens
    // Intersection: {alpha, beta, gamma} = 3
    // Containment = 3 / min(4, 4) = 3/4 = 0.75 >= 0.7 floor → flagged as "likely"
    let exact_boundary1 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "alpha beta gamma delta" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let exact_boundary2 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "alpha beta gamma epsilon" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Call find_duplicate_memories with min_similarity 0.6 (should include the boundary pair, which has containment 0.75).
    let result = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({ "min_similarity": 0.6 }),
        DEFAULT_TIMEOUT,
    );

    // Check that we found the pair with containment 0.75 similarity.
    let pairs = result["pairs"].as_array().unwrap();
    assert!(
        !pairs.is_empty(),
        "should find at least 1 pair with containment 0.75: {result:#?}"
    );

    let pair = &pairs[0];
    let ids = pair["ids"].as_array().unwrap();
    let pair_ids_set = (
        ids[0].as_str().unwrap().to_string(),
        ids[1].as_str().unwrap().to_string(),
    );
    let expected_pair = if exact_boundary1 <= exact_boundary2 {
        (exact_boundary1.clone(), exact_boundary2.clone())
    } else {
        (exact_boundary2.clone(), exact_boundary1.clone())
    };

    assert_eq!(
        pair_ids_set, expected_pair,
        "should find the 0.75-containment boundary pair: {result:#?}"
    );
    assert!(
        (pair["similarity"].as_f64().unwrap() - 0.75).abs() < 0.001,
        "pair similarity should be approximately 0.75 (containment): {result:#?}"
    );
}

#[test]
fn duplicate_scan_dispatches_on_embedder_status_not_stored_vectors() {
    // TDD item 1: duplicate_scan must dispatch on embedder.status(), not on
    // whether stored embeddings exist. With a Failed embedder, the text path is
    // taken even if set_embedding adds vectors.
    let dir = tempfile::tempdir().unwrap();
    let scope = topodb::ScopeId::new().to_string();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &[
            "--scope",
            scope.as_str(),
            "--embeddings",
            "not-a-real-model",
        ],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Create two overlapping memories
    let mem1 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({"content": "the login flow breaks when the session cookie exceeds four kilobytes"}),
        DEFAULT_TIMEOUT,
    );
    let mem1_id = mem1["id"].as_str().unwrap();

    let mem2 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({"content": "the login flow breaks when the session cookie exceeds the size limit"}),
        DEFAULT_TIMEOUT,
    );
    let mem2_id = mem2["id"].as_str().unwrap();

    // Check the model name from db_info to use in set_embedding
    let db_info = s.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT);
    let model_name = db_info["embeddings"]["model"].as_str().unwrap();

    // Set an embedding on mem1 with the failed model name
    let small_vector = vec![0.1, 0.2, 0.3, 0.4];
    s.call_tool_ok(
        "set_embedding",
        serde_json::json!({
            "id": mem1_id,
            "model": model_name,
            "vector": small_vector
        }),
        DEFAULT_TIMEOUT,
    );

    // find_duplicate_memories must still use text mode because embedder.status() is Failed
    let result = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({}),
        DEFAULT_TIMEOUT,
    );

    assert_eq!(
        result["method"], "text",
        "dispatch must use text mode when embedder status is Failed, regardless of stored embeddings: {result}"
    );
    assert!(
        !result["pairs"].as_array().unwrap().is_empty(),
        "text mode should find the overlapping pair: {result}"
    );

    // The pair should use text-mode similarity (token-Jaccard containment)
    let pair = &result["pairs"][0];
    let ids = [
        pair["ids"][0].as_str().unwrap(),
        pair["ids"][1].as_str().unwrap(),
    ];
    assert!(
        ids.contains(&mem1_id) && ids.contains(&mem2_id),
        "pair should include both memories: {pair}"
    );
}

#[test]
fn text_mode_ignores_min_similarity_threshold() {
    // Item 6: min_similarity is ignored in text mode. A pair with token-containment
    // >= 0.7 is returned even with min_similarity: 0.99.
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(DEFAULT_TIMEOUT);

    // Create a pair with high token containment (>= 0.7 floor)
    let mem1_id = s.call_tool_ok(
        "create_memory",
        serde_json::json!({"content": "the login breaks when the cookie exceeds size"}),
        DEFAULT_TIMEOUT,
    );
    let mem1_id = mem1_id["id"].as_str().unwrap();

    let mem2_id = s.call_tool_ok(
        "create_memory",
        serde_json::json!({"content": "the login breaks when the cookie exceeds four"}),
        DEFAULT_TIMEOUT,
    );
    let mem2_id = mem2_id["id"].as_str().unwrap();

    // Call find_duplicate_memories with min_similarity: 0.99 (very high).
    // In text mode, this should be ignored; the containment pair (>= 0.7) should still
    // be returned.
    let result = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({"min_similarity": 0.99}),
        DEFAULT_TIMEOUT,
    );

    assert_eq!(
        result["method"], "text",
        "should use text mode since embeddings are off: {result}"
    );
    assert!(
        !result["pairs"].as_array().unwrap().is_empty(),
        "text mode should return the containment pair despite min_similarity: 0.99: {result}"
    );

    let pair = &result["pairs"][0];
    let ids = [
        pair["ids"][0].as_str().unwrap(),
        pair["ids"][1].as_str().unwrap(),
    ];
    assert!(
        ids.contains(&mem1_id) && ids.contains(&mem2_id),
        "pair should include both memories: {pair}"
    );
}
