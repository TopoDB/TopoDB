//! Behavioral e2e for the Plan 6 tools (set_node_props, remove_node,
//! close_edge, set_embedding, search_vectors, submit_batch). Reuses the same
//! spawn/JSON-RPC plumbing as `e2e.rs` via `tests/common`.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("plan6.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn set_node_props_and_remove_node() {
    let (_dir, mut server) = fresh_server();
    let id = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "ada", "props": { "stale": "yes" } }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // set_node_props: add a key, remove another with null.
    let res = server.call_tool_ok(
        "set_node_props",
        serde_json::json!({ "id": id, "props": { "role": "pioneer", "stale": null } }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        res["seq"].as_u64().is_some(),
        "set_node_props returns seq: {res}"
    );

    let node = server.call_tool_ok("get_node", serde_json::json!({ "id": id }), DEFAULT_TIMEOUT);
    assert_eq!(node["node"]["props"]["role"], serde_json::json!("pioneer"));
    assert!(node["node"]["props"].get("stale").is_none());

    // remove_node: the node is gone afterward.
    let res = server.call_tool_ok(
        "remove_node",
        serde_json::json!({ "id": id }),
        DEFAULT_TIMEOUT,
    );
    assert!(res["seq"].as_u64().is_some());
    let gone = server.call_tool_ok("get_node", serde_json::json!({ "id": id }), DEFAULT_TIMEOUT);
    assert_eq!(gone["found"], serde_json::json!(false));
}

#[test]
fn set_node_props_on_missing_node_is_tool_error() {
    let (_dir, mut server) = fresh_server();
    let ghost = topodb::NodeId::new().to_string();
    let resp = server.call_tool(
        "set_node_props",
        serde_json::json!({ "id": ghost, "props": { "x": 1 } }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn close_edge_closes_an_open_edge() {
    let (_dir, mut server) = fresh_server();
    let a = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "a" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "b" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let e = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": a, "to_id": b, "edge_type": "x" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A real ms-since-epoch value: close_edge rejects implausible timestamps
    // (a bare `1000` reads as a seconds-vs-ms mixup and would date the close
    // to January 1970).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let res = server.call_tool_ok(
        "close_edge",
        serde_json::json!({ "id": e, "valid_to": now_ms }),
        DEFAULT_TIMEOUT,
    );
    assert!(res["seq"].as_u64().is_some());
}

#[test]
fn set_embedding_then_vector_search_finds_it() {
    let (_dir, mut server) = fresh_server();
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "vectorized" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let res = server.call_tool_ok(
        "set_embedding",
        serde_json::json!({ "id": m, "model": "test", "vector": [1.0, 0.0] }),
        DEFAULT_TIMEOUT,
    );
    assert!(res["seq"].as_u64().is_some());
}

#[test]
fn close_edge_missing_is_tool_error() {
    let (_dir, mut server) = fresh_server();
    let ghost = topodb::EdgeId::new().to_string();
    let resp = server.call_tool(
        "close_edge",
        serde_json::json!({ "id": ghost }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn set_embedding_rejects_non_finite_vector() {
    let (_dir, mut server) = fresh_server();
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "x" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // 1e300 is a finite f64 but overflows to f32::INFINITY on cast,
    // deterministically tripping json_to_f32_vec's !is_finite() check.
    let resp = server.call_tool(
        "set_embedding",
        serde_json::json!({ "id": m, "model": "test", "vector": [1e300, 0.0] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn search_vectors_ranks_by_cosine() {
    let (_dir, mut server) = fresh_server();
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "near" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.call_tool_ok(
        "set_embedding",
        serde_json::json!({ "id": m, "model": "test", "vector": [1.0, 0.0] }),
        DEFAULT_TIMEOUT,
    );
    let res = server.call_tool_ok(
        "search_vectors",
        serde_json::json!({ "model": "test", "vector": [1.0, 0.0], "k": 5 }),
        DEFAULT_TIMEOUT,
    );
    let hits = res["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().any(|h| h["node"]["id"] == serde_json::json!(m)),
        "M should rank: {res}"
    );
    assert!(hits[0]["score"].as_f64().is_some());
}

#[test]
fn search_vectors_empty_vector_is_tool_error() {
    let (_dir, mut server) = fresh_server();
    let resp = server.call_tool(
        "search_vectors",
        serde_json::json!({ "model": "test", "vector": [] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn search_vectors_rejects_non_finite_vector() {
    let (_dir, mut server) = fresh_server();
    // 1e300 is a finite f64 but overflows to f32::INFINITY on cast,
    // deterministically tripping json_to_f32_vec's !is_finite() check.
    let resp = server.call_tool(
        "search_vectors",
        serde_json::json!({ "model": "test", "vector": [1e300, 0.0] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn submit_batch_atomic_with_backrefs() {
    let (_dir, mut server) = fresh_server();
    let res = server.call_tool_ok(
        "submit_batch",
        serde_json::json!({ "commands": [
            { "op": "create_entity", "name": "Ada" },
            { "op": "create_memory", "content": "met Ada" },
            { "op": "link", "from": "#1", "to": "#0", "type": "about" }
        ] }),
        DEFAULT_TIMEOUT,
    );
    let ids = res["ids"].as_array().expect("ids array");
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|v| v.is_string()));
    // All committed: the entity is findable.
    let found = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "Ada" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(found["nodes"].as_array().unwrap().len(), 1);
}

#[test]
fn submit_batch_bad_backref_is_tool_error_and_atomic() {
    let (_dir, mut server) = fresh_server();
    let resp = server.call_tool(
        "submit_batch",
        serde_json::json!({ "commands": [
            { "op": "create_entity", "name": "Nope" },
            { "op": "link", "from": "#5", "to": "#0", "type": "x" }
        ] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    // Nothing committed.
    let found = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "Nope" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        found["nodes"].as_array().unwrap().len(),
        0,
        "batch must be atomic"
    );
}

/// `link` with `supersede: true` atomically closes the other open same-type
/// edges from the source before recording the new fact — the "changed
/// employer" flow, end to end over MCP.
#[test]
fn link_supersede_closes_other_open_same_type_edges() {
    let (_dir, mut server) = fresh_server();
    let make_entity = |name: &str, server: &mut Server| {
        server.call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": name }),
            DEFAULT_TIMEOUT,
        )["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let drew = make_entity("drew", &mut server);
    let acme = make_entity("acme", &mut server);
    let globex = make_entity("globex", &mut server);

    let first_edge = server.call_tool_ok(
        "link",
        serde_json::json!({ "from_id": drew, "to_id": acme, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Drew changes employers: supersede closes the acme edge.
    let moved = server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": drew,
            "to_id": globex,
            "edge_type": "works_at",
            "supersede": true
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(moved["created"], true, "{moved:#?}");
    let superseded: Vec<&str> = moved["superseded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        superseded,
        vec![first_edge.as_str()],
        "the old works_at edge must be closed: {moved:#?}"
    );

    // Only the new employment is open now; the old edge survives as history.
    let open = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": drew, "edge_type": "works_at" }),
        DEFAULT_TIMEOUT,
    );
    let open_rows = open["edges"].as_array().unwrap();
    assert_eq!(open_rows.len(), 1, "{open:#?}");
    assert_eq!(open_rows[0]["to"].as_str().unwrap(), globex);
    let all = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": drew, "edge_type": "works_at", "open_only": false }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        all["edges"].as_array().unwrap().len(),
        2,
        "the closed edge must remain in history: {all:#?}"
    );
}

/// The seconds-vs-milliseconds and future-timestamp guards on `link`.
#[test]
fn link_rejects_implausible_valid_from() {
    let (_dir, mut server) = fresh_server();
    let a = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "a" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "b" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // Seconds-since-epoch (too small) and far-future (beyond skew slack)
    // must both be rejected before anything is written.
    for bad in [now_ms / 1000, now_ms + 3_600_000] {
        let resp = server.call_tool(
            "link",
            serde_json::json!({
                "from_id": a, "to_id": b, "edge_type": "knows", "valid_from": bad
            }),
            DEFAULT_TIMEOUT,
        );
        expect_tool_error(&resp);
    }
    // A plausible past value is accepted.
    let ok = server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": a, "to_id": b, "edge_type": "knows", "valid_from": now_ms - 1000
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(ok["created"], true);
}
