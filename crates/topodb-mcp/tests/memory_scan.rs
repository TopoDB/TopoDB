//! Maintenance scan for existing near-duplicate memories: `find_duplicate_memories`
//! sweeps the memories already stored in the read scopes and reports pairs that are
//! semantically close (cosine `>=` the same near-dup floor write-time detection
//! uses), most-similar first. Read-only and advisory — nothing is merged; the
//! caller decides what to supersede. Where `near_duplicates` catches a redundancy
//! at write time, this finds the ones that slipped in before the check existed
//! (or across a scope the writer didn't compare against).
//!
//! The real-embedder test is #[ignore] (downloads ~34MB), matching
//! tests/memory_near_dup.rs; the off-path guard and validation run in CI.

mod common;

use common::{Server, DEFAULT_TIMEOUT};

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

#[test]
fn find_duplicate_memories_uses_text_fallback_when_embeddings_are_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    // Two disjoint facts — with embeddings off, text-based detection (token-Jaccard)
    // should still run but find no matches below the text threshold.
    s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the auth service uses JWT tokens" }),
        DEFAULT_TIMEOUT,
    );
    s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "office plants need water on Thursdays" }),
        DEFAULT_TIMEOUT,
    );

    let res = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({}),
        DEFAULT_TIMEOUT,
    );
    // Method is "text" when embeddings are off (text fallback active).
    assert_eq!(res["method"].as_str().unwrap(), "text", "{res}");
    // No pairs because the memories are disjoint (low Jaccard similarity).
    assert_eq!(
        res["pairs"],
        serde_json::json!([]),
        "disjoint memories should not pair even with text fallback: {res}"
    );
    // Scanned must reflect the actual number of memories examined.
    assert_eq!(res["scanned"], 2, "must scan all memories stored: {res}");
    assert_eq!(res["truncated"], false, "{res}");
}

#[test]
fn find_duplicate_memories_rejects_out_of_range_similarity() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(
        &dir.path().join("t.redb"),
        &["--scope", A, "--embeddings", "off"],
    );
    s.initialize(DEFAULT_TIMEOUT);

    let resp = s.call_tool(
        "find_duplicate_memories",
        serde_json::json!({ "min_similarity": 1.5 }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);

    let resp = s.call_tool(
        "find_duplicate_memories",
        serde_json::json!({ "min_similarity": -0.1 }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&resp);
}

/// Requires the real embedder (ONNX Runtime + a model download). Run locally:
///   cargo test -p topodb-mcp --test memory_scan -- --ignored
#[ignore]
#[test]
fn scan_surfaces_a_pre_existing_near_duplicate_pair() {
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

    // Two lexically different but semantically identical facts. Neither create
    // call flags the other IF they were written before the near-dup check —
    // this scan is what catches them after the fact.
    let a = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the authentication service issues JWT tokens to sign in users" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "auth uses JSON Web Tokens to authenticate and log people in" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // An unrelated fact that must NOT pair with either.
    let _c = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the office plants are watered on Tuesdays" }),
        DEFAULT_TIMEOUT,
    );

    let res = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({}),
        DEFAULT_TIMEOUT,
    );
    let pairs = res["pairs"].as_array().expect("pairs array");
    assert_eq!(
        res["scanned"], 3,
        "all three embedded memories scanned: {res}"
    );

    // Exactly the {a, b} pair, in either id order, above the 0.80 floor.
    let ab = pairs.iter().find(|p| {
        let ids = p["ids"].as_array().unwrap();
        let set: std::collections::HashSet<_> = ids.iter().map(|v| v.as_str().unwrap()).collect();
        set == [a.as_str(), b.as_str()].into_iter().collect()
    });
    let ab = ab.unwrap_or_else(|| panic!("the a/b fact pair should be reported: {res:#?}"));
    assert!(
        ab["similarity"].as_f64().unwrap() >= 0.80,
        "pair similarity should clear the 0.80 floor: {ab}"
    );

    // The unrelated fact must not appear in ANY pair.
    for p in pairs {
        let ids = p["ids"].as_array().unwrap();
        assert!(
            ids.iter().all(|v| v != &_c["id"]),
            "the unrelated plant fact must not pair with anything: {res:#?}"
        );
    }

    // A symmetric pair is reported once, not twice.
    assert_eq!(
        pairs.len(),
        1,
        "one unordered pair, reported once: {res:#?}"
    );
}

/// Requires the real embedder. Bands + relation labels: a reworded duplicate is
/// tagged `duplicate`, a contradicting pair (same subject, one negates the other)
/// is tagged `supersession` — even though cosine scores the contradiction just as
/// high. Run: cargo test -p topodb-mcp --test memory_scan -- --ignored
#[ignore]
#[test]
fn scan_labels_band_and_distinguishes_supersession_from_duplicate() {
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let info = s.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT);
        match info["embeddings"]["status"].as_str().unwrap() {
            "ready" => break,
            "failed" | "off" => panic!("embedder not usable: {info:#?}"),
            _ => {
                assert!(std::time::Instant::now() < deadline, "model never ready");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // A same-fact reworded pair (should be `duplicate`).
    let d1 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "TopoDB uses redb as its storage backend" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let d2 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "The storage engine behind TopoDB is redb" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A contradicting pair (should be `supersession`).
    let c1 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the auth service issues JWT tokens" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c2 = s.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the auth service now issues opaque session tokens, not JWTs" }),
        DEFAULT_TIMEOUT,
    )["id"].as_str().unwrap().to_string();

    let res = s.call_tool_ok(
        "find_duplicate_memories",
        serde_json::json!({}),
        DEFAULT_TIMEOUT,
    );
    let pairs = res["pairs"].as_array().unwrap();
    let find = |x: &str, y: &str| {
        pairs.iter().find(|p| {
            let ids: std::collections::HashSet<_> = p["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            ids == [x, y].into_iter().collect()
        })
    };
    let dup = find(&d1, &d2).unwrap_or_else(|| panic!("redb duplicate pair missing: {res:#?}"));
    assert_eq!(
        dup["relation"], "duplicate",
        "reworded same fact => duplicate: {dup}"
    );
    assert_eq!(dup["band"], "likely", "0.9+ => likely: {dup}");

    let sup = find(&c1, &c2).unwrap_or_else(|| panic!("auth contradiction pair missing: {res:#?}"));
    assert_eq!(
        sup["relation"], "supersession",
        "contradiction => supersession: {sup}"
    );
}
