//! `graph_snapshot`: exports a renderable subgraph (ego view from seed/query,
//! or a whole-scope view) as inline mermaid, a full JSON snapshot, or a
//! self-contained HTML file. The CLI's daemon-routed `graph` command calls
//! this tool with `format: "json"` and deserializes the result straight into
//! `topodb_json::GraphSnapshot`, so the json format must return the full
//! snapshot object verbatim (see `emit_graph`/`Command::Graph` in
//! `topodb-cli/src/main.rs`).

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

#[test]
fn auto_format_inlines_mermaid_for_a_small_graph() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    // One memory linked to one entity — a small scope-view graph.
    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "ALPHA-FACT about the alpha thing", "entities": ["Alpha"] }),
        DEFAULT_TIMEOUT,
    );

    let result = server.call_tool_ok(
        "graph_snapshot",
        serde_json::json!({ "format": "auto" }),
        DEFAULT_TIMEOUT,
    );
    let mermaid = result["mermaid"]
        .as_str()
        .expect("small graph inlines mermaid");
    assert!(
        mermaid.starts_with("graph TD"),
        "mermaid should start with 'graph TD': {mermaid}"
    );
    assert!(
        result["nodes"].as_u64().unwrap() >= 2,
        "should see at least the entity + memory node: {result:#?}"
    );
}

#[test]
fn json_format_returns_the_full_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "ALPHA-FACT about the alpha thing", "entities": ["Alpha"] }),
        DEFAULT_TIMEOUT,
    );

    let result = server.call_tool_ok(
        "graph_snapshot",
        serde_json::json!({ "format": "json" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(result["view"]["kind"], "scope");
    assert_eq!(result["snapshot_version"], 1);
    // db_path is stamped by the server, not left null.
    assert!(result["db_path"].as_str().is_some(), "{result:#?}");
}

#[test]
fn html_format_requires_out_and_writes_a_self_contained_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "ALPHA-FACT about the alpha thing", "entities": ["Alpha"] }),
        DEFAULT_TIMEOUT,
    );

    // Missing `out` is a clean error.
    let resp = server.call_tool(
        "graph_snapshot",
        serde_json::json!({ "format": "html" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    let out = dir.path().join("g.html");
    let result = server.call_tool_ok(
        "graph_snapshot",
        serde_json::json!({ "format": "html", "out": out.to_str().unwrap() }),
        DEFAULT_TIMEOUT,
    );
    assert!(result["path"].as_str().unwrap().ends_with("g.html"));
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("id=\"snapshot\""), "{html}");
}

#[test]
fn auto_format_without_out_on_a_large_graph_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    // Enough entities to exceed GRAPH_MERMAID_INLINE_MAX_NODES (60) via a
    // scope view — no seed/query needed.
    for i in 0..65 {
        server.call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": format!("Node{i}") }),
            DEFAULT_TIMEOUT,
        );
    }

    let resp = server.call_tool(
        "graph_snapshot",
        serde_json::json!({ "format": "auto" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn ego_view_from_seed_reaches_the_linked_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "ALPHA-FACT about the alpha thing", "entities": ["Alpha"] }),
        DEFAULT_TIMEOUT,
    );
    let alpha = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "Alpha" }),
        DEFAULT_TIMEOUT,
    )["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = server.call_tool_ok(
        "graph_snapshot",
        serde_json::json!({ "seeds": [alpha], "max_hops": 1, "format": "json" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(result["view"]["kind"], "ego");
    let blob = result["nodes"].to_string();
    assert!(
        blob.contains("Alpha"),
        "ego view should reach the seed entity: {result:#?}"
    );
}

#[test]
fn bad_format_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    let resp = server.call_tool(
        "graph_snapshot",
        serde_json::json!({ "format": "yaml" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn seedless_query_with_no_hits_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("t.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);

    let resp = server.call_tool(
        "graph_snapshot",
        serde_json::json!({ "query": "nothing matches this at all" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}
