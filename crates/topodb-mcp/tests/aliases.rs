//! Alias + synonym behavioral tests over the real binary (stdio).
mod common;
use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("aliases.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

fn entity(server: &mut Server, name: &str) -> String {
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
fn alias_resolves_to_canonical_entity_everywhere() {
    let (_dir, mut server) = fresh_server();
    let drew = entity(&mut server, "Drew Powell");

    let added = server.call_tool_ok(
        "add_alias",
        serde_json::json!({ "entity_id": drew, "alias": "Drew" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(added["created"], true);

    // create_entity with the alias name resolves to the CANONICAL entity.
    let resolved = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "drew" }), // case variant of the alias
        DEFAULT_TIMEOUT,
    );
    assert_eq!(resolved["id"].as_str().unwrap(), drew);
    assert_eq!(resolved["created"], false);

    // find_by_prop on Entity/name with the alias also resolves canonical.
    let found = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "Drew" }),
        DEFAULT_TIMEOUT,
    );
    let ids: Vec<&str> = found["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![drew.as_str()]);

    // Idempotent: same alias -> created:false, same alias node id.
    let again = server.call_tool_ok(
        "add_alias",
        serde_json::json!({ "entity_id": drew, "alias": "Drew" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(again["created"], false);
    assert_eq!(again["id"], added["id"]);
}

#[test]
fn add_alias_conflict_and_bad_target_are_errors() {
    let (_dir, mut server) = fresh_server();
    let a = entity(&mut server, "Alpha");
    let b = entity(&mut server, "Beta");

    // Alias equal to a DIFFERENT entity's name: merge conflict, rejected.
    let resp = server.call_tool(
        "add_alias",
        serde_json::json!({ "entity_id": a, "alias": "Beta" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    let _ = b;

    // Target must be an Entity.
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "not an entity" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = server.call_tool(
        "add_alias",
        serde_json::json!({ "entity_id": m, "alias": "nickname" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn synonym_expands_search_and_graph_boost_pulls_neighbors() {
    let (_dir, mut server) = fresh_server();
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "login page rework details" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Before the synonym: "auth" finds nothing.
    let empty = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "auth", "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(empty["hits"].as_array().unwrap().len(), 0);

    server.call_tool_ok(
        "add_synonym",
        serde_json::json!({ "term": "auth", "expansion": "login" }),
        DEFAULT_TIMEOUT,
    );

    // After: the login memory surfaces for "auth".
    let hits = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "auth", "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    let ids: Vec<&str> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![m.as_str()]);

    // Bidirectional default: "login" query ALSO expands to "auth".
    let m2 = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "auth token refresh bug" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let hits2 = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "login", "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    let ids2: Vec<&str> = hits2["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["id"].as_str().unwrap())
        .collect();
    assert!(ids2.contains(&m.as_str()) && ids2.contains(&m2.as_str()));

    // add_synonym rejects term == expansion.
    let resp = server.call_tool(
        "add_synonym",
        serde_json::json!({ "term": "auth", "expansion": "AUTH" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn search_memories_graph_boost_param_controls_neighbor_pull() {
    let (_dir, mut server) = fresh_server();
    let hit = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "deployment pipeline broke friday" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let linked = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "rollback procedure revert redeploy" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": linked, "to_id": hit, "edge_type": "about" }),
        DEFAULT_TIMEOUT,
    );
    let with = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "deployment friday" }),
        DEFAULT_TIMEOUT,
    );
    let ids: Vec<&str> = with["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&linked.as_str()),
        "graph_boost default-on pulls the neighbor"
    );

    let without = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "deployment friday", "graph_boost": false }),
        DEFAULT_TIMEOUT,
    );
    let ids2: Vec<&str> = without["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["id"].as_str().unwrap())
        .collect();
    assert!(!ids2.contains(&linked.as_str()));
}
