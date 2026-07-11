//! Multi-scope reads (P1): a client can read across a project scope *and*
//! `shared` in one call, and an edge can be stamped into an explicit scope so
//! the graph actually crosses a scope boundary.
//!
//! Shared spawn/JSON-RPC/deadline plumbing lives in `tests/common/mod.rs` —
//! see that module's docs for why every read is deadlined and the child is
//! always killed.

mod common;

use common::{Server, DEFAULT_TIMEOUT};

/// A memory in scope A and a memory in `shared` are BOTH visible to a reader
/// whose set is {A, shared} — the capability that did not exist before P1.
#[test]
fn read_spans_project_scope_and_shared() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("multi.redb");
    let project = topodb::ScopeId::new().to_string();
    let read_list = format!("{project},shared");

    let mut server = Server::spawn(
        &db_path,
        &["--scope", project.as_str(), "--read-scopes", read_list.as_str()],
    );
    server.initialize(DEFAULT_TIMEOUT);

    // Written with no explicit scope => the default WRITE scope => project.
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "zzqqx project fact" }),
        DEFAULT_TIMEOUT,
    );
    // Written explicitly into shared.
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "zzqqx shared lesson", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );

    // The default read set is {project, shared} => both come back.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "zzqqx", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let hits = res["hits"].as_array().expect("hits should be an array");
    assert_eq!(
        hits.len(),
        2,
        "a {{project, shared}} read set must see BOTH memories, got: {hits:?}"
    );
}

/// Per-call `scopes` overrides the server default, and beats `scope`.
#[test]
fn per_call_scopes_param_overrides_the_default_and_beats_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("percall.redb");
    let project = topodb::ScopeId::new().to_string();

    // Default read set is project-only (no --read-scopes).
    let mut server = Server::spawn(&db_path, &["--scope", project.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);

    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "wwvvu project fact" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "wwvvu shared lesson", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );

    // Default (project-only) sees 1.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "wwvvu", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(res["hits"].as_array().unwrap().len(), 1);

    // Explicit multi-scope read sees both. `scopes` wins over `scope`.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({
            "query": "wwvvu",
            "k": 10,
            "scope": "shared",
            "scopes": [project.as_str(), "shared"]
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        res["hits"].as_array().unwrap().len(),
        2,
        "`scopes` must take precedence over `scope`"
    );
}

/// THE REGRESSION TEST for the Task 2 bug: an edge explicitly stamped `shared`
/// must be traversable by a reader whose set includes `shared` — i.e. the graph
/// crosses a scope boundary. Before the `link` fix this edge would have been
/// stamped with the project scope and been invisible from any other project.
#[test]
fn a_shared_edge_is_traversable_from_a_multi_scope_reader() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("edge.redb");
    let project = topodb::ScopeId::new().to_string();
    let read_list = format!("{project},shared");

    let mut server = Server::spawn(
        &db_path,
        &["--scope", project.as_str(), "--read-scopes", read_list.as_str()],
    );
    server.initialize(DEFAULT_TIMEOUT);

    // Two nodes in `shared`, linked by an edge explicitly stamped `shared`.
    let a = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "shared-a", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let b = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "shared-b", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": a_id, "to_id": b_id, "edge_type": "about", "scope": "shared"
        }),
        DEFAULT_TIMEOUT,
    );

    // Traverse from A across `shared` — the edge must be visible.
    // NB: traverse's params are `seed_id` / `max_hops` (see TraverseParams,
    // server.rs:287), and its result is `{ "subgraph": {...} }`.
    let res = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "max_hops": 1, "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    let body = res["subgraph"].to_string();
    assert!(
        body.contains(&b_id),
        "a `shared`-scoped edge must be traversable by a reader of `shared`; \
         got subgraph: {body}"
    );
}
