mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("remember_conflicts.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn contradicting_restatement_surfaces_a_supersession_candidate() {
    let (_dir, mut server) = fresh_server();
    let first = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB stores its data in redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    let first_id = first["memory_id"].as_str().unwrap().to_string();
    assert!(first.get("supersession_candidates").is_none());

    let second = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB now stores its data in sled, not redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    // Advisory invariant: the write itself succeeded regardless of the probe.
    let second_id = second["memory_id"].as_str().unwrap();
    assert_ne!(second_id, first_id);

    let candidates = second["supersession_candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("expected supersession_candidates on: {second}"));
    assert_eq!(candidates.len(), 1, "candidates: {candidates:?}");
    assert_eq!(candidates[0]["memory_id"].as_str().unwrap(), first_id);
    assert_eq!(candidates[0]["relation"].as_str().unwrap(), "supersession");
    assert!(candidates[0]["score"].as_f64().unwrap() > 0.0);
}

#[test]
fn same_pair_restatement_surfaces_a_duplicate_candidate() {
    let (_dir, mut server) = fresh_server();
    let first = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the auth service issues JWT access tokens for every request",
            "entities": ["Auth Service"],
        }),
        DEFAULT_TIMEOUT,
    );
    let first_id = first["memory_id"].as_str().unwrap().to_string();

    // High-containment rewording (~0.90) without negation cues, meeting text-mode
    // fallback threshold (TEXT_NEAR_DUP_CONTAINMENT = 0.7). Must differ enough
    // to avoid dedup. When embeddings are off or not ready, text fallback applies.
    let second = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the auth service issues JWT access tokens for each request",
            "entities": ["Auth Service"],
        }),
        DEFAULT_TIMEOUT,
    );
    let candidates = second["supersession_candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("expected supersession_candidates on: {second}"));
    assert_eq!(candidates.len(), 1, "candidates: {candidates:?}");
    assert_eq!(candidates[0]["memory_id"].as_str().unwrap(), first_id);
    assert_eq!(candidates[0]["relation"].as_str().unwrap(), "duplicate");
}

#[test]
fn unrelated_fact_has_no_candidates_field() {
    let (_dir, mut server) = fresh_server();
    server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB stores its data in redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    let unrelated = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "The office coffee machine needs descaling",
            "entities": ["Office"],
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        unrelated.get("supersession_candidates").is_none(),
        "{unrelated}"
    );
}

#[test]
fn explicit_supersedes_excludes_that_id_from_supersession_candidates() {
    let (_dir, mut server) = fresh_server();
    let first = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB stores its data in redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    let first_id = first["memory_id"].as_str().unwrap().to_string();

    // The near-duplicate probe runs BEFORE submit_write (it needs the pre-write
    // graph), so without filtering it would list `first_id` as a candidate even
    // though this same call's own `supersedes` already tombstones it. The
    // projection must exclude ids this call is itself superseding.
    let second = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB now stores its data in sled, not redb",
            "entities": ["TopoDB"],
            "supersedes": [first_id],
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(second.get("supersession_candidates").is_none(), "{second}");
    let superseded = second["superseded"]
        .as_array()
        .unwrap_or_else(|| panic!("expected superseded on: {second}"));
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0].as_str().unwrap(), first_id);
    // Advisory invariant: the write still succeeded.
    assert!(second["memory_id"].as_str().is_some());
}

#[test]
fn check_conflicts_false_skips_the_probe_even_for_a_contradicting_pair() {
    let (_dir, mut server) = fresh_server();
    server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the auth service issues JWT tokens",
            "entities": ["Auth Service"],
        }),
        DEFAULT_TIMEOUT,
    );
    let second = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the auth service now issues opaque session tokens, not JWTs",
            "entities": ["Auth Service"],
            "check_conflicts": false,
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(second.get("supersession_candidates").is_none(), "{second}");
    // Advisory invariant: the write still succeeded.
    assert!(second["memory_id"].as_str().is_some());
}
