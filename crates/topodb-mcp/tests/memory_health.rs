//! `memory_health`: one call that runs all three hygiene scans (duplicate,
//! orphan, stale) and returns a consolidated summary — the "what needs attention
//! in my memory?" entry point an agent can run at session start instead of
//! remembering three separate maintenance tools. Read-only; composes the exact
//! same cores as find_duplicate/orphan/stale_memories, so definitions never
//! drift. The CI-path tests use no embedder (duplicate_pairs is then 0 and
//! embeddings_enabled is false); the real-embedder path is #[ignore].

mod common;

use common::{Server, DEFAULT_TIMEOUT as T};
use serde_json::json;

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

fn fresh() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(T);
    (dir, s)
}

fn id_of(v: &serde_json::Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

#[test]
fn a_tidy_store_needs_no_attention() {
    let (_d, mut s) = fresh();
    let topic = id_of(&s.call_tool_ok("create_entity", json!({"name": "Topic"}), T));
    // One memory, linked (not an orphan) and freshly created (not stale).
    let m = id_of(&s.call_tool_ok(
        "create_memory",
        json!({"content": "a linked fresh fact"}),
        T,
    ));
    s.call_tool_ok(
        "link",
        json!({"from_id": m, "to_id": topic, "edge_type": "about"}),
        T,
    );

    let res = s.call_tool_ok("memory_health", json!({}), T);
    assert_eq!(res["total_memories"], 1, "{res}");
    assert_eq!(res["orphan_count"], 0, "{res}");
    assert_eq!(res["stale_count"], 0, "fresh at the 30-day default: {res}");
    assert_eq!(res["duplicate_pairs"], 0, "{res}");
    assert_eq!(
        res["embeddings_enabled"], false,
        "spawned --embeddings off: {res}"
    );
    assert_eq!(res["needs_attention"], false, "nothing wrong: {res}");
}

#[test]
fn surfaces_counts_and_samples_when_there_is_work() {
    let (_d, mut s) = fresh();
    // Two orphans (bare create_memory, never linked).
    let o1 = id_of(&s.call_tool_ok("create_memory", json!({"content": "orphan one"}), T));
    let _o2 = id_of(&s.call_tool_ok("create_memory", json!({"content": "orphan two"}), T));
    // One linked memory (not an orphan).
    let topic = id_of(&s.call_tool_ok("create_entity", json!({"name": "Topic"}), T));
    let linked = id_of(&s.call_tool_ok("create_memory", json!({"content": "linked fact"}), T));
    s.call_tool_ok(
        "link",
        json!({"from_id": linked, "to_id": topic, "edge_type": "about"}),
        T,
    );

    // Threshold 0 => every live memory counts as stale.
    let res = s.call_tool_ok("memory_health", json!({"stale_older_than_days": 0}), T);
    assert_eq!(res["total_memories"], 3, "three live memories: {res}");
    assert_eq!(res["orphan_count"], 2, "the two unlinked ones: {res}");
    assert_eq!(res["stale_count"], 3, "all live at threshold 0: {res}");
    // Embeddings off => no near-dup signal at all, so both split counts are 0.
    assert_eq!(res["duplicate_pairs"], 0, "{res}");
    assert_eq!(res["supersession_pairs"], 0, "{res}");
    assert_eq!(res["needs_attention"], true, "{res}");

    // Samples are present and point at real ids.
    let orphan_sample: Vec<&str> = res["sample_orphans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert!(
        orphan_sample.contains(&o1.as_str()),
        "sample names an orphan: {res}"
    );
    assert!(!res["sample_stale"].as_array().unwrap().is_empty(), "{res}");
    // Samples are bounded (a summary, not the full lists).
    assert!(
        res["sample_orphans"].as_array().unwrap().len() <= 3,
        "{res}"
    );
    assert!(res["sample_stale"].as_array().unwrap().len() <= 3, "{res}");
}

#[test]
fn empty_store_and_bounds() {
    let (_d, mut s) = fresh();
    let res = s.call_tool_ok("memory_health", json!({}), T);
    assert_eq!(res["total_memories"], 0, "{res}");
    assert_eq!(res["needs_attention"], false, "{res}");
    assert_eq!(res["sample_orphans"].as_array().unwrap().len(), 0, "{res}");

    common::expect_tool_error(&s.call_tool(
        "memory_health",
        json!({"stale_older_than_days": -1}),
        T,
    ));
}

/// Requires the real embedder. Run: cargo test -p topodb-mcp --test memory_health -- --ignored
#[ignore]
#[test]
fn reports_duplicate_pairs_when_embeddings_are_on() {
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
    s.initialize(T);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let info = s.call_tool_ok("db_info", json!({}), T);
        match info["embeddings"]["status"].as_str().unwrap() {
            "ready" => break,
            "failed" | "off" => panic!("embedder not usable: {info:#?}"),
            _ => {
                assert!(std::time::Instant::now() < deadline, "model never ready");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // Two lexically different but semantically identical facts.
    s.call_tool_ok(
        "create_memory",
        json!({"content": "the auth service issues JWT tokens to sign in users"}),
        T,
    );
    s.call_tool_ok(
        "create_memory",
        json!({"content": "auth uses JSON Web Tokens to authenticate and log people in"}),
        T,
    );

    let res = s.call_tool_ok("memory_health", json!({}), T);
    assert_eq!(res["embeddings_enabled"], true, "{res}");
    assert!(
        res["duplicate_pairs"].as_u64().unwrap() >= 1,
        "the near-dup pair should be counted: {res}"
    );
    assert!(
        !res["sample_duplicates"].as_array().unwrap().is_empty(),
        "and appear in the sample: {res}"
    );
    assert_eq!(res["needs_attention"], true, "{res}");
}

/// Requires the real embedder. A contradicting pair must land in
/// `supersession_pairs`, not `duplicate_pairs`.
/// Run: cargo test -p topodb-mcp --test memory_health -- --ignored
#[ignore]
#[test]
fn splits_supersessions_out_of_duplicate_count() {
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
    s.initialize(T);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let info = s.call_tool_ok("db_info", json!({}), T);
        match info["embeddings"]["status"].as_str().unwrap() {
            "ready" => break,
            "failed" | "off" => panic!("embedder not usable: {info:#?}"),
            _ => {
                assert!(std::time::Instant::now() < deadline, "model never ready");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    // Contradicting facts about the same subject (cosine >= 0.80, but a negation).
    s.call_tool_ok(
        "create_memory",
        json!({"content": "the auth service issues JWT tokens"}),
        T,
    );
    s.call_tool_ok(
        "create_memory",
        json!({"content": "the auth service now issues opaque session tokens, not JWTs"}),
        T,
    );

    let res = s.call_tool_ok("memory_health", json!({}), T);
    assert_eq!(res["embeddings_enabled"], true, "{res}");
    assert!(
        res["supersession_pairs"].as_u64().unwrap() >= 1,
        "the contradiction must count as a supersession: {res}"
    );
    assert_eq!(res["duplicate_pairs"], 0, "and NOT as a duplicate: {res}");
    assert_eq!(res["needs_attention"], true, "{res}");
}
