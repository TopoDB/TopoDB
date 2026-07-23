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
