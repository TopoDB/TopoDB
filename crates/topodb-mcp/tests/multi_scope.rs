//! Multi-scope reads (P1): a client can read across a project scope *and*
//! `shared` in one call, and an edge can be stamped into an explicit scope so
//! the graph actually crosses a scope boundary.
//!
//! Shared spawn/JSON-RPC/deadline plumbing lives in `tests/common/mod.rs` —
//! see that module's docs for why every read is deadlined and the child is
//! always killed.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

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
        &[
            "--scope",
            project.as_str(),
            "--read-scopes",
            read_list.as_str(),
        ],
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
        &[
            "--scope",
            project.as_str(),
            "--read-scopes",
            read_list.as_str(),
        ],
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
    // NB: traverse's params are `seed_id` / `max_hops` (see TraverseParams
    // in server.rs), and its result is `{ "subgraph": {...} }`.
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

/// THE REGRESSION TEST for the review finding: `scopes` is a genuine param
/// name on the six READ tools but was never wired to the WRITE tools — before
/// `#[serde(deny_unknown_fields)]`, a write call carrying `scopes` (e.g. an
/// agent generalising the read pattern to "write across scopes") was silently
/// deserialized with `scopes` dropped on the floor and the write landed in
/// the default project scope anyway, reporting success. That is the same
/// quiet-failure family as the `link`-scope bug this branch exists to fix, so
/// it must now be a clean tool error, AND the memory must genuinely not have
/// been created (not just "erred but wrote anyway").
#[test]
fn create_memory_rejects_unknown_scopes_field_instead_of_silently_ignoring_it() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("deny_unknown.redb");
    let mut server = Server::spawn(&db_path, &[]);
    server.initialize(DEFAULT_TIMEOUT);

    let resp = server.call_tool(
        "create_memory",
        serde_json::json!({ "content": "probeqqq fact", "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    let message = tool_error_message(&resp);
    assert!(
        message.contains("scopes"),
        "the rejection should name the unknown `scopes` field so a caller can \
         tell `scopes` isn't a write-tool param (write tools take one `scope`, \
         not a `scopes` set), got: {resp:#?}"
    );

    // Not merely erred — genuinely never wrote anything, not even into the
    // default scope. A default-scope search for the unique probe content
    // must come back empty.
    let found = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "probeqqq" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        found["hits"].as_array().unwrap().len(),
        0,
        "create_memory with an unknown `scopes` field must not silently create \
         a project-scoped memory: {found:#?}"
    );
}

/// Extracts a tool error's message text, regardless of which of the two
/// shapes [`expect_tool_error`] accepts it landed on (see that helper's doc
/// comment): a top-level JSON-RPC `error.message`, or `result.content[0].text`
/// for the `isError: true` case. Lets a test assert on *what* the rejection
/// says, not just that a rejection happened.
fn tool_error_message(resp: &serde_json::Value) -> String {
    if let Some(msg) = resp
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return msg.to_string();
    }
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string()
}

/// THE REGRESSION TEST for the empty-`scopes` constraint: `resolve_scopes`'s
/// `Some([])` arm (server.rs) rejects an explicitly empty `scopes: []` rather
/// than silently treating it as "read everything" — there is no unscoped
/// read, so an empty set must be a caller error. This is verified today for
/// all six read tools, but until now nothing pinned it: a future refactor
/// that deleted that arm would compile, pass every other test, and quietly
/// turn "reject" into "admit nothing" or "read the default set" without a
/// single test noticing.
///
/// Every case's `id`/`seed_id` (where required) is a syntactically valid,
/// freshly-minted ULID that need not actually exist — `resolve_scopes` runs
/// before any id lookup in every one of these tools (see each tool's body in
/// server.rs), so the rejection observed here is genuinely the `scopes` one,
/// not an unrelated "no such node" error dressed up the same way.
#[test]
fn empty_scopes_is_rejected_by_every_read_tool() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("empty_scopes.redb");
    let project = topodb::ScopeId::new().to_string();

    let mut server = Server::spawn(&db_path, &["--scope", project.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);

    let node_id = topodb::NodeId::new().to_string();

    let cases: [(&str, serde_json::Value); 6] = [
        (
            "get_node",
            serde_json::json!({ "id": node_id, "scopes": [] }),
        ),
        (
            "find_by_prop",
            serde_json::json!({
                "label": "Entity", "prop": "name", "value": "ada", "scopes": []
            }),
        ),
        (
            "search_memories",
            serde_json::json!({ "query": "ada", "scopes": [] }),
        ),
        (
            "traverse",
            serde_json::json!({ "seed_id": node_id, "scopes": [] }),
        ),
        (
            "access_stats",
            serde_json::json!({ "id": node_id, "scopes": [] }),
        ),
        (
            "search_vectors",
            serde_json::json!({ "model": "test", "vector": [1.0], "scopes": [] }),
        ),
    ];

    for (tool, args) in cases {
        let resp = server.call_tool(tool, args, DEFAULT_TIMEOUT);
        expect_tool_error(&resp);
        let message = tool_error_message(&resp);
        assert!(
            message.contains("scopes") && message.contains("empty"),
            "{tool}'s `scopes: []` error should mention `scopes` being empty \
             (so a future error-message change can't silently turn rejection \
             into acceptance unnoticed), got: {resp:#?}"
        );
    }
}

/// Table-driven: with a server whose DEFAULT read set is project-only and
/// the fixture data written into `shared`, every one of the six read tools
/// must see NOTHING by default and see the data once `scopes: ["shared"]` is
/// passed. Closes the coverage gap the reviewer flagged: before this test
/// only `search_memories` and `traverse` were exercised by a committed test
/// (per [`per_call_scopes_param_overrides_the_default_and_beats_scope`] and
/// [`a_shared_edge_is_traversable_from_a_multi_scope_reader`]), and even
/// `traverse`'s existing coverage used a default read set that already
/// included `shared` — never the "invisible by default" half of the
/// precedence this test pins for all six.
#[test]
fn all_six_read_tools_honour_scopes_default_vs_override() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("all_six.redb");
    let project = topodb::ScopeId::new().to_string();

    // No `--read-scopes` => the default read set is project-only.
    let mut server = Server::spawn(&db_path, &["--scope", project.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);

    // --- Fixture: nodes/edge/embedding, ALL stamped `shared` ---------------
    let entity = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "zzzscope-entity", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let entity_id = entity["id"].as_str().unwrap().to_string();

    let other = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "zzzscope-entity-2", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let other_id = other["id"].as_str().unwrap().to_string();

    server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": entity_id, "to_id": other_id, "edge_type": "about", "scope": "shared"
        }),
        DEFAULT_TIMEOUT,
    );

    let memory = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "zzzscope memory content", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let memory_id = memory["id"].as_str().unwrap().to_string();

    // `set_embedding` takes no `scope` param — the embedding attaches to the
    // node by id, so it lives wherever the node itself does (`shared`).
    server.call_tool_ok(
        "set_embedding",
        serde_json::json!({ "id": memory_id, "model": "zzzscope-model", "vector": [1.0, 0.0] }),
        DEFAULT_TIMEOUT,
    );

    // --- get_node ------------------------------------------------------
    let default = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": entity_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        default["found"], false,
        "get_node's default (project-only) read set must not see a `shared` node: {default:#?}"
    );
    let overridden = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": entity_id, "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        overridden["found"], true,
        "get_node with scopes: [\"shared\"] must see the node: {overridden:#?}"
    );

    // --- access_stats ----------------------------------------------------
    let default = server.call_tool_ok(
        "access_stats",
        serde_json::json!({ "id": entity_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        default["found"], false,
        "access_stats's default (project-only) read set must not see a `shared` node: {default:#?}"
    );
    let overridden = server.call_tool_ok(
        "access_stats",
        serde_json::json!({ "id": entity_id, "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        overridden["found"], true,
        "access_stats with scopes: [\"shared\"] must see the node: {overridden:#?}"
    );

    // --- find_by_prop ------------------------------------------------------
    let default = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "zzzscope-entity" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        default["nodes"].as_array().unwrap().len(),
        0,
        "find_by_prop's default (project-only) read set must find 0 nodes in \
         `shared`: {default:#?}"
    );
    let overridden = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({
            "label": "Entity", "prop": "name", "value": "zzzscope-entity", "scopes": ["shared"]
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        overridden["nodes"].as_array().unwrap().len(),
        1,
        "find_by_prop with scopes: [\"shared\"] must find the node: {overridden:#?}"
    );

    // --- search_memories -------------------------------------------------
    let default = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "zzzscope", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        default["hits"].as_array().unwrap().len(),
        0,
        "search_memories's default (project-only) read set must find 0 hits \
         in `shared`: {default:#?}"
    );
    let overridden = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "zzzscope", "k": 10, "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        overridden["hits"].as_array().unwrap().len(),
        1,
        "search_memories with scopes: [\"shared\"] must find the memory: {overridden:#?}"
    );

    // --- traverse ----------------------------------------------------------
    let default = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": entity_id, "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let default_body = default["subgraph"].to_string();
    assert!(
        !default_body.contains(&other_id),
        "traverse's default (project-only) read set must not reach the \
         `shared` edge: {default_body}"
    );
    let overridden = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": entity_id, "max_hops": 1, "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    let overridden_body = overridden["subgraph"].to_string();
    assert!(
        overridden_body.contains(&other_id),
        "traverse with scopes: [\"shared\"] must reach the `shared` edge: {overridden_body}"
    );

    // --- search_vectors ----------------------------------------------------
    let default = server.call_tool_ok(
        "search_vectors",
        serde_json::json!({ "model": "zzzscope-model", "vector": [1.0, 0.0], "k": 5 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        default["hits"].as_array().unwrap().len(),
        0,
        "search_vectors's default (project-only) read set must find 0 hits \
         in `shared`: {default:#?}"
    );
    let overridden = server.call_tool_ok(
        "search_vectors",
        serde_json::json!({
            "model": "zzzscope-model", "vector": [1.0, 0.0], "k": 5, "scopes": ["shared"]
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        overridden["hits"].as_array().unwrap().len(),
        1,
        "search_vectors with scopes: [\"shared\"] must find the embedding: {overridden:#?}"
    );
}

/// `get_changes` is the one unscoped read — it spans every scope in the db.
/// In a db shared across projects that is a cross-project leak, so it is off
/// unless the host explicitly opts in.
#[test]
fn get_changes_is_gated_unless_explicitly_allowed() {
    use common::expect_tool_error;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("gate.redb");

    // Without the flag: the call is a tool error naming the flag.
    let mut server = Server::spawn(&db_path, &["--scope", "shared"]);
    server.initialize(DEFAULT_TIMEOUT);
    let resp = server.call_tool(
        "get_changes",
        serde_json::json!({ "since_seq": 0 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    let body = resp.to_string();
    assert!(
        body.contains("--allow-unscoped-changes"),
        "the error must name the flag that enables it; got: {body}"
    );
    drop(server); // release the db file before reopening it below

    // With the flag: it works.
    let mut server = Server::spawn(&db_path, &["--scope", "shared", "--allow-unscoped-changes"]);
    server.initialize(DEFAULT_TIMEOUT);
    let res = server.call_tool_ok(
        "get_changes",
        serde_json::json!({ "since_seq": 0 }),
        DEFAULT_TIMEOUT,
    );
    assert!(res.get("ops").is_some());
}
