//! Bi-temporal edges: `time_axis` on `get_edges` and `traverse`, plus the
//! belief-axis JSON fields (`recorded_at`/`superseded_at`) on `get_edges`.
//!
//! Scenario: link A->X with a backdated `valid_from` (JUNE), while the
//! server's real wall clock is "now" (August) — so `recorded_at` ends up
//! far later than `valid_from`. That gap is what separates the two axes:
//! - valid axis: was the edge true in the world at `as_of`? (`valid_from`
//!   <= as_of < valid_to)
//! - recorded axis: had we WRITTEN the edge down by `as_of`? (`recorded_at`
//!   <= as_of < superseded_at)
//!
//! A late-recorded fact (backdated valid_from) is present on the valid axis
//! for a past `as_of` inside its validity window, but absent on the
//! recorded axis for that same `as_of` — because the write itself hadn't
//! happened yet.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

/// 2026-06-01T00:00:00Z in Unix ms.
const JUNE: i64 = 1_780_300_800_000;
/// 2026-07-01T00:00:00Z in Unix ms.
const JULY: i64 = 1_782_864_000_000;

/// Spawn a server (in `dir`), create A and X, link A->X with `valid_from:
/// JUNE`, and return `(server, a_id, x_id, edge_id)`. The server's real
/// clock is "now" (well after JULY), so the edge's `recorded_at` lands far
/// after its backdated `valid_from`. `dir` is borrowed from the caller so it
/// (and the db file it holds) outlives the returned `Server`, matching this
/// suite's existing `seeded_server(&dir, ...)` convention.
fn setup_backdated_edge(dir: &tempfile::TempDir) -> (Server, String, String, String) {
    let db_path = dir.path().join("edges_axis.redb");
    let scope = topodb::ScopeId::new().to_string();
    let scope_args = ["--scope", scope.as_str(), "--allow-unscoped-changes"];

    let mut server = Server::spawn(&db_path, &scope_args);
    server.initialize(DEFAULT_TIMEOUT);

    let a_id = server
        .call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": "node_a" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return id")
        .to_string();

    let x_id = server
        .call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": "node_x" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return id")
        .to_string();

    let edge_id = server
        .call_tool_ok(
            "link",
            serde_json::json!({
                "from_id": a_id,
                "to_id": x_id,
                "edge_type": "relates_to",
                "valid_from": JUNE
            }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("link should return id")
        .to_string();

    (server, a_id, x_id, edge_id)
}

#[test]
fn get_edges_default_axis_is_valid_and_json_carries_belief_fields() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x_id, edge_id) = setup_backdated_edge(&dir);

    // Default (no as_of, no time_axis): present, and JSON carries both
    // belief fields with recorded_at far after valid_from.
    let result = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a_id }),
        DEFAULT_TIMEOUT,
    );
    let edges = result["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 1, "expected exactly one edge: {edges:?}");
    let e = &edges[0];
    assert_eq!(e["id"], serde_json::Value::String(edge_id.clone()));
    assert_eq!(e["valid_from"], serde_json::json!(JUNE));
    let recorded_at = e["recorded_at"]
        .as_i64()
        .expect("recorded_at should be a number");
    assert!(
        recorded_at > JUNE,
        "recorded_at ({recorded_at}) should be far later than the backdated valid_from ({JUNE})"
    );
    assert_eq!(e["superseded_at"], serde_json::Value::Null);
}

#[test]
fn get_edges_as_of_july_valid_axis_present() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x_id, edge_id) = setup_backdated_edge(&dir);

    let result = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a_id, "as_of": JULY }),
        DEFAULT_TIMEOUT,
    );
    let edges = result["edges"].as_array().expect("edges array");
    let ids: Vec<&str> = edges.iter().filter_map(|e| e["id"].as_str()).collect();
    assert!(
        ids.contains(&edge_id.as_str()),
        "valid-axis as_of=JULY should see the edge (JUNE <= JULY): {edges:?}"
    );
}

/// Review follow-up (final whole-branch review, Minor #4): CLI already has
/// the "explicit default == implicit default" case
/// (`crates/topodb-cli/tests/cli.rs`'s `get_edges_time_axis_default_valid_and_belief_fields_present`/
/// `get_edges_time_axis_valid_as_of_july_present` pair); MCP had no case for
/// an explicit `"time_axis": "valid"` before this test. Low risk
/// (`server.rs` maps `None | Some("valid")` identically), but the suites
/// should be symmetric where the surfaces are.
#[test]
fn get_edges_as_of_july_explicit_valid_axis_matches_implicit_default() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x_id, edge_id) = setup_backdated_edge(&dir);

    let implicit = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a_id, "as_of": JULY }),
        DEFAULT_TIMEOUT,
    );
    let explicit = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a_id, "as_of": JULY, "time_axis": "valid" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        implicit, explicit,
        "omitting time_axis must match explicit \"valid\" byte-for-byte"
    );
    let ids: Vec<&str> = explicit["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert!(
        ids.contains(&edge_id.as_str()),
        "explicit valid-axis as_of=JULY should see the edge (JUNE <= JULY): {explicit:?}"
    );
}

#[test]
fn get_edges_as_of_july_recorded_axis_absent() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x_id, _edge_id) = setup_backdated_edge(&dir);

    let result = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": a_id, "as_of": JULY, "time_axis": "recorded" }),
        DEFAULT_TIMEOUT,
    );
    let edges = result["edges"].as_array().expect("edges array");
    assert!(
        edges.is_empty(),
        "recorded-axis as_of=JULY should NOT see the edge — it wasn't written yet: {edges:?}"
    );
}

#[test]
fn get_edges_invalid_time_axis_is_tool_error_naming_both_values() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x_id, _edge_id) = setup_backdated_edge(&dir);

    let resp = server.call_tool(
        "get_edges",
        serde_json::json!({ "from_id": a_id, "time_axis": "nonsense" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    let msg = resp.to_string();
    assert!(
        msg.contains("valid") && msg.contains("recorded"),
        "error should name both accepted values: {msg}"
    );
}

#[test]
fn traverse_default_axis_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, x_id, _edge_id) = setup_backdated_edge(&dir);

    let result = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let nodes = result["subgraph"]["nodes"]
        .as_array()
        .expect("subgraph nodes");
    let ids: Vec<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    assert!(
        ids.contains(&x_id.as_str()),
        "default (valid axis, now) traverse should reach X: {ids:?}"
    );
}

#[test]
fn traverse_as_of_july_valid_axis_present() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, x_id, _edge_id) = setup_backdated_edge(&dir);

    let result = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "max_hops": 1, "as_of": JULY }),
        DEFAULT_TIMEOUT,
    );
    let nodes = result["subgraph"]["nodes"]
        .as_array()
        .expect("subgraph nodes");
    let ids: Vec<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    assert!(
        ids.contains(&x_id.as_str()),
        "valid-axis as_of=JULY traverse should reach X (JUNE <= JULY): {ids:?}"
    );
}

#[test]
fn traverse_as_of_july_recorded_axis_absent() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, x_id, _edge_id) = setup_backdated_edge(&dir);

    let result = server.call_tool_ok(
        "traverse",
        serde_json::json!({
            "seed_id": a_id,
            "max_hops": 1,
            "as_of": JULY,
            "time_axis": "recorded"
        }),
        DEFAULT_TIMEOUT,
    );
    let nodes = result["subgraph"]["nodes"]
        .as_array()
        .expect("subgraph nodes");
    let ids: Vec<&str> = nodes.iter().filter_map(|n| n["id"].as_str()).collect();
    assert!(
        !ids.contains(&x_id.as_str()),
        "recorded-axis as_of=JULY traverse should NOT reach X — the edge wasn't written yet: {ids:?}"
    );
}

#[test]
fn traverse_invalid_time_axis_is_tool_error_naming_both_values() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x_id, _edge_id) = setup_backdated_edge(&dir);

    let resp = server.call_tool(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "time_axis": "nonsense" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    let msg = resp.to_string();
    assert!(
        msg.contains("valid") && msg.contains("recorded"),
        "error should name both accepted values: {msg}"
    );
}
