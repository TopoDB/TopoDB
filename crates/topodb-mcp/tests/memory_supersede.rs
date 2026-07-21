//! Supersession: when a fact changes, `remember(..., supersedes: [old_id])`
//! retires the old memory — it stops surfacing in search/traverse — without
//! deleting it. Explicit (the agent names what it replaces); history-preserving
//! (the node is marked, not removed).

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

fn entity_id(s: &mut Server, name: &str) -> String {
    let r = s.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": name }),
        DEFAULT_TIMEOUT,
    );
    r["nodes"][0]["id"].as_str().unwrap().to_string()
}

#[test]
fn superseded_memory_stops_surfacing_in_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(DEFAULT_TIMEOUT);

    let old = s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "the auth service uses JWT tokens", "entities": ["Auth"] }),
        DEFAULT_TIMEOUT,
    );
    let old_id = old["memory_id"].as_str().unwrap().to_string();

    // The fact changed: store the new memory and supersede the old one.
    let new = s.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the auth service uses PASETO tokens",
            "entities": ["Auth"],
            "supersedes": [old_id],
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        new["superseded"],
        serde_json::json!([old_id]),
        "the call must report the retired memory: {new}"
    );

    // Search now returns the current fact (PASETO) and NOT the retired one (JWT).
    let hits = s.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "auth service tokens", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let blob = serde_json::to_string(&hits).unwrap();
    assert!(blob.contains("PASETO"), "current fact must surface: {hits}");
    assert!(
        !blob.contains("JWT"),
        "the superseded fact must not resurface in search: {hits}"
    );
}

#[test]
fn superseded_memory_is_unlinked_from_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(DEFAULT_TIMEOUT);

    let old = s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "deploy target is Heroku", "entities": ["Deploy"] }),
        DEFAULT_TIMEOUT,
    );
    let old_id = old["memory_id"].as_str().unwrap().to_string();
    s.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "deploy target is Fly.io",
            "entities": ["Deploy"],
            "supersedes": [old_id],
        }),
        DEFAULT_TIMEOUT,
    );

    // Traversing from the entity reaches the current fact, not the retired one.
    let deploy = entity_id(&mut s, "Deploy");
    let tr = s.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": deploy, "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let blob = serde_json::to_string(&tr["subgraph"]).unwrap();
    assert!(
        blob.contains("Fly.io"),
        "current fact reachable: {}",
        tr["subgraph"]
    );
    assert!(
        !blob.contains("Heroku"),
        "the superseded memory's link is closed, so open traversal must not reach it: {}",
        tr["subgraph"]
    );
}

#[test]
fn supersedes_rejects_a_non_memory_or_unknown_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(DEFAULT_TIMEOUT);

    // An entity id (not a Memory) is rejected.
    s.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "x", "entities": ["Thing"] }),
        DEFAULT_TIMEOUT,
    );
    let thing = entity_id(&mut s, "Thing");
    let resp = s.call_tool(
        "remember",
        serde_json::json!({ "content": "y", "entities": ["Thing"], "supersedes": [thing] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // An unknown id is rejected — a bad supersedes must write nothing.
    let unknown = "01HZY0ZZZZZZZZZZZZZZZZZZZZZ";
    let resp2 = s.call_tool(
        "remember",
        serde_json::json!({ "content": "z", "entities": ["Thing"], "supersedes": [unknown] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp2);
}
