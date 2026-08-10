mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("e2e.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

fn create_entity(server: &mut Server, name: &str) -> String {
    server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": name }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn acceptance_1_link_conflicts_then_supersede_clears_them() {
    let (_dir, mut server) = fresh_server();
    let a = create_entity(&mut server, "Person A");
    let x = create_entity(&mut server, "Company X");
    let y = create_entity(&mut server, "Company Y");
    let z = create_entity(&mut server, "Company Z");

    let ax = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": x, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    );
    let ax_id = ax["id"].as_str().unwrap().to_string();

    let ay = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": y, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    );
    let conflicts = ay["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["edge_id"].as_str().unwrap(), ax_id);

    let az = server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": a, "to_id": z, "edge_type": "works_at", "supersede": true
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(az.get("conflicts").is_none(), "{az}");
    let closed: Vec<&str> = az["superseded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(closed.len(), 2, "both ax and ay should now be closed: {az}");

    let open = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a }),
        DEFAULT_TIMEOUT,
    );
    let open_ids: Vec<&str> = open["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(open_ids.len(), 1, "only the az edge remains open: {open}");
}

#[test]
fn acceptance_2_remember_supersession_duplicate_unrelated_and_opt_out() {
    let (_dir, mut server) = fresh_server();
    let base = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB stores its data in redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    let base_id = base["memory_id"].as_str().unwrap().to_string();

    // CONTRADICT pair (from dup_classify_tests) -> supersession.
    let contradicting = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB now stores its data in sled, not redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        contradicting["memory_id"].as_str().is_some(),
        "write succeeded: {contradicting}"
    );
    let cand = contradicting["supersession_candidates"].as_array().unwrap();
    assert_eq!(cand.len(), 1);
    assert_eq!(cand[0]["memory_id"].as_str().unwrap(), base_id);
    assert_eq!(cand[0]["relation"].as_str().unwrap(), "supersession");

    // SAME-pair rewording of a DIFFERENT fact -> duplicate.
    let base2 = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "Python is a dynamically typed language",
            "entities": ["Python"],
        }),
        DEFAULT_TIMEOUT,
    );
    let base2_id = base2["memory_id"].as_str().unwrap().to_string();
    let reworded = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "Python is dynamically typed",
            "entities": ["Python"],
        }),
        DEFAULT_TIMEOUT,
    );
    let cand2 = reworded["supersession_candidates"].as_array().unwrap();
    assert!(
        cand2
            .iter()
            .any(|c| c["memory_id"].as_str().unwrap() == base2_id
                && c["relation"].as_str().unwrap() == "duplicate"),
        "{cand2:?}"
    );

    // Unrelated fact -> field absent.
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

    // check_conflicts: false -> field absent even for a contradicting pair.
    let opted_out = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "the auth service now issues opaque session tokens, not JWTs",
            "entities": ["Auth Service"],
            "check_conflicts": false,
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        opted_out.get("supersession_candidates").is_none(),
        "{opted_out}"
    );
    assert!(
        opted_out["memory_id"].as_str().is_some(),
        "write still succeeded: {opted_out}"
    );
}

#[test]
fn acceptance_4_probe_never_bumps_access_stats() {
    let (_dir, mut server) = fresh_server();
    let base = server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB stores its data in redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );
    let base_id = base["memory_id"].as_str().unwrap().to_string();

    let before = server.call_tool_ok(
        "access_stats",
        serde_json::json!({ "id": base_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(before["access_count"].as_u64().unwrap(), 0);

    // This remember's probe reads `base` as a candidate — must not bump it.
    server.call_tool_ok(
        "remember",
        serde_json::json!({
            "content": "TopoDB now stores its data in sled, not redb",
            "entities": ["TopoDB"],
        }),
        DEFAULT_TIMEOUT,
    );

    let after = server.call_tool_ok(
        "access_stats",
        serde_json::json!({ "id": base_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        after["access_count"].as_u64().unwrap(),
        0,
        "the advisory probe must use the unbumped search path: {after}"
    );
}
