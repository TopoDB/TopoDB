//! Semantic near-duplicate detection: on storing a NEW memory, the write tools
//! surface existing memories that are semantically close (advisory — nothing is
//! merged), so an agent can notice "same fact, different words" that exact dedup
//! misses. Off/not-ready embeddings degrade to an empty list (no false signal).
//!
//! The real-embedder test is #[ignore] (downloads ~34MB), matching
//! tests/embeddings.rs; the off-path guard runs in CI.

mod common;

use common::{Server, DEFAULT_TIMEOUT};

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

#[test]
fn near_duplicates_is_empty_when_embeddings_are_off() {
    let dir = tempfile::tempdir().unwrap();
    // Default spawn is already --embeddings off; be explicit.
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    let cm = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the auth service uses JWT tokens" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        cm["near_duplicates"],
        serde_json::json!([]),
        "no embeddings => no semantic signal, must be an empty list: {cm}"
    );

    let rm = s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "auth issues JSON Web Tokens", "entities": ["Auth"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(rm["near_duplicates"], serde_json::json!([]), "{rm}");
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
    let sim = near
        .iter()
        .find(|n| n["id"] == original)
        .and_then(|n| n["similarity"].as_f64())
        .unwrap();
    assert!(
        sim >= 0.80,
        "similarity should clear the 0.80 near-dup floor, got {sim}"
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
