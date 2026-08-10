mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("link.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

fn create_entity(server: &mut Server, name: &str) -> String {
    let res = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": name }),
        DEFAULT_TIMEOUT,
    );
    res["id"].as_str().unwrap().to_string()
}

#[test]
fn second_open_same_type_edge_surfaces_the_first_as_a_conflict() {
    let (_dir, mut server) = fresh_server();
    let a = create_entity(&mut server, "Drew Powell");
    let x = create_entity(&mut server, "Anthropic");
    let y = create_entity(&mut server, "TopoDB");

    let first = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": x, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    );
    let first_edge_id = first["id"].as_str().unwrap().to_string();
    assert!(
        first.get("conflicts").is_none(),
        "first edge has nothing to conflict with: {first}"
    );

    let second = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": y, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    );
    assert_ne!(
        second["id"].as_str().unwrap(),
        first_edge_id,
        "a distinct edge was created"
    );
    let conflicts = second["conflicts"]
        .as_array()
        .unwrap_or_else(|| panic!("expected conflicts on: {second}"));
    assert_eq!(conflicts.len(), 1, "conflicts: {conflicts:?}");
    assert_eq!(conflicts[0]["edge_id"].as_str().unwrap(), first_edge_id);
    assert_eq!(conflicts[0]["to"].as_str().unwrap(), x);
    assert!(conflicts[0]["valid_from"].is_i64());
}

#[test]
fn supersede_true_closes_conflicts_instead_of_reporting_them() {
    let (_dir, mut server) = fresh_server();
    let a = create_entity(&mut server, "Drew Powell");
    let x = create_entity(&mut server, "Anthropic");
    let z = create_entity(&mut server, "TopoDB");

    let first = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": x, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    );
    let first_edge_id = first["id"].as_str().unwrap().to_string();

    let superseding = server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": a, "to_id": z, "edge_type": "works_at", "supersede": true
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        superseding.get("conflicts").is_none(),
        "supersede: true must not report conflicts: {superseding}"
    );
    let superseded = superseding["superseded"].as_array().unwrap();
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0].as_str().unwrap(), first_edge_id);

    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a }),
        DEFAULT_TIMEOUT,
    );
    let open: Vec<&str> = edges["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(
        !open.contains(&first_edge_id.as_str()),
        "first edge is closed: {edges}"
    );
}
