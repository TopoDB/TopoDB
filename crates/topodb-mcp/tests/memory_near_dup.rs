//! Semantic near-duplicate detection: on storing a NEW memory, the write tools
//! surface existing memories that are semantically close (advisory — nothing is
//! merged), so an agent can notice "same fact, different words" that exact dedup
//! misses. When embeddings are Ready, vector-based detection applies; when not
//! ready, text-based fallback (token-Jaccard containment) applies.
//!
//! The real-embedder test is #[ignore] (downloads ~34MB), matching
//! tests/embeddings.rs; the off-path guard runs in CI.

mod common;

use common::{Server, DEFAULT_TIMEOUT};

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

#[test]
fn text_fallback_flags_overlapping_content() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Create the original memory.
    let _original = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the login flow breaks when the session cookie exceeds four kilobytes" }),
        DEFAULT_TIMEOUT,
    );

    // Create a similar memory with significant token overlap (>0.7 containment).
    let similar = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the login flow breaks when the session cookie exceeds the size limit" }),
        DEFAULT_TIMEOUT,
    );

    let near = similar["near_duplicates"]
        .as_array()
        .expect("near_duplicates should be an array");
    assert!(
        !near.is_empty(),
        "text fallback should surface overlapping content: {similar:#?}"
    );

    let first_hit = &near[0];
    assert!(
        first_hit["method"].as_str().unwrap() == "text",
        "method should be 'text' when embeddings are off: {first_hit:#?}"
    );
    assert!(
        first_hit["similarity"].as_f64().unwrap() > 0.0
            && first_hit["similarity"].as_f64().unwrap() <= 1.0,
        "similarity score should be between 0 and 1: {first_hit:#?}"
    );
}

#[test]
fn text_fallback_quiet_on_disjoint_content() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Create an initial memory.
    let _original = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the authentication system uses OAuth2 tokens" }),
        DEFAULT_TIMEOUT,
    );

    // Create a completely unrelated memory.
    let unrelated = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the office plants need water on Thursdays" }),
        DEFAULT_TIMEOUT,
    );

    let near = unrelated["near_duplicates"]
        .as_array()
        .expect("near_duplicates should be an array");
    assert!(
        near.is_empty(),
        "text fallback should not surface disjoint content: {unrelated:#?}"
    );
}

/// Requires the real embedder (ONNX Runtime + a model download). Run locally:
///   cargo test -p topodb-mcp --test memory_near_dup -- --ignored
#[ignore]
#[test]
fn semantically_similar_fact_is_surfaced_as_a_near_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let scope = topodb::ScopeId::new().to_string();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &[
            "--scope",
            scope.as_str(),
            "--embeddings",
            "bge-small-en-v1.5",
        ],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Wait for the model to load (download on first run; cached after).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let info = s.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT);
        match info["embeddings"]["status"].as_str().unwrap() {
            "ready" => break,
            "failed" => panic!("model failed to load: {info:#?}"),
            "off" => panic!("embedder off — spawn args failed to override the default"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "model never became ready"
                );
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    let original = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the authentication service issues JWT tokens to sign in users" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A lexically DIFFERENT but semantically SAME fact — exact dedup can't catch
    // this; the near-duplicate check should.
    let similar = s.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "auth uses JSON Web Tokens to authenticate and log people in",
            "entities": ["Auth"],
        }),
        DEFAULT_TIMEOUT,
    );
    let near = similar["near_duplicates"]
        .as_array()
        .expect("near_duplicates array");
    assert!(
        near.iter().any(|n| n["id"] == original),
        "the original fact should surface as a near-duplicate: {similar:#?}"
    );
    let dup = near
        .iter()
        .find(|n| n["id"] == original)
        .expect("original should be in near_duplicates");
    let sim = dup["similarity"].as_f64().unwrap();
    assert!(
        sim >= 0.80,
        "similarity should clear the 0.80 near-dup floor, got {sim}"
    );
    assert!(
        dup["method"].as_str().unwrap() == "vector",
        "method should be 'vector' when embeddings are ready: {dup:#?}"
    );

    // An UNRELATED fact must NOT surface the auth memory (guards false merges).
    let unrelated = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the office plants are watered on Tuesdays" }),
        DEFAULT_TIMEOUT,
    );
    let near_unrelated = unrelated["near_duplicates"].as_array().unwrap();
    assert!(
        !near_unrelated.iter().any(|n| n["id"] == original),
        "an unrelated fact must not be flagged similar to the auth memory: {unrelated:#?}"
    );
}

/// Canonical pair from the near-dup metric rewrite: "Vega stores its data in postgres"
/// vs "Vega now stores its data in sqlite for embedded mode". Under Jaccard,
/// they score ~0.455 (uncatchable with 0.5 floor). Under CONTAINMENT, they score
/// 5/6 ≈ 0.833 (caught); band is "likely" (>= 0.80 threshold).
#[test]
fn text_fallback_canonical_containment_pair() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Original memory with six tokens: {vega, stores, its, data, in, postgres}.
    let _original = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "Vega stores its data in postgres" }),
        DEFAULT_TIMEOUT,
    );

    // Restatement with ten tokens: {vega, now, stores, its, data, in, sqlite, for, embedded, mode}.
    // Intersection: {vega, stores, its, data, in} = 5 tokens.
    // Containment = 5 / min(6, 10) = 5/6 ≈ 0.8333 >= 0.7 floor → caught as "likely".
    let similar = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "Vega now stores its data in sqlite for embedded mode" }),
        DEFAULT_TIMEOUT,
    );

    let near = similar["near_duplicates"]
        .as_array()
        .expect("near_duplicates should be an array");
    assert!(
        !near.is_empty(),
        "text fallback should surface the canonical pair (containment ≈0.833): {similar:#?}"
    );

    let first_hit = &near[0];
    assert_eq!(
        first_hit["method"].as_str().unwrap(),
        "text",
        "method should be 'text' when embeddings are off"
    );
    assert_eq!(
        first_hit["band"].as_str().unwrap(),
        "likely",
        "band should be 'likely' since 0.8333 >= 0.80 threshold: {first_hit:#?}"
    );
    let similarity = first_hit["similarity"].as_f64().unwrap();
    assert!(
        (similarity - 0.8333).abs() < 0.01,
        "similarity should be ≈0.8333 (5/6), got {similarity}: {first_hit:#?}"
    );
}

#[test]
fn text_fallback_detects_exact_subset_at_1_0_containment() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Create the first memory.
    let _original = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "alpha beta gamma delta" }),
        DEFAULT_TIMEOUT,
    );

    // Create a memory with 1.0 containment (exact subset).
    // Set 1: {alpha, beta, gamma, delta} = 4 tokens
    // Set 2: {alpha, beta, gamma, delta, zeta, eta, theta} = 7 tokens
    // Intersection: 4, min(|A|, |B|) = 4, Containment = 4/4 = 1.0 >= 0.7 floor
    let similar = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "alpha beta gamma delta zeta eta theta" }),
        DEFAULT_TIMEOUT,
    );

    let near = similar["near_duplicates"]
        .as_array()
        .expect("near_duplicates should be an array");
    assert!(
        !near.is_empty(),
        "text fallback should surface content with containment 1.0 (exact subset): {similar:#?}"
    );

    let first_hit = &near[0];
    assert!(
        first_hit["method"].as_str().unwrap() == "text",
        "method should be 'text' when embeddings are off: {first_hit:#?}"
    );
}
