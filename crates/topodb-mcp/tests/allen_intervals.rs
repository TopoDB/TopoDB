//! Allen interval predicates on `get_edges` and `traverse` (semantica
//! completion, spec: docs/superpowers/specs/2026-08-11-semantica-completion-
//! design.md, Item 3): the four mutually-exclusive `valid_during` /
//! `valid_overlaps` / `valid_before` / `valid_after` params over the
//! half-open edge valid interval `[valid_from, valid_to)`.
//!
//! Fixture: A->X backdated to JUNE and left OPEN; A->Y backdated to JUNE and
//! CLOSED at JULY. The pair separates every predicate:
//! - `during [JUNE, JULY]`: only the closed edge (an open edge never
//!   satisfies containment; `valid_to == b` passes the half-open bound).
//! - `overlaps [JULY, AUG]`: only the open edge (the closed one ends
//!   exactly at `a`, and `overlaps` requires `valid_to > a`).
//! - `before JULY`: only the closed edge (fully over by then).
//! - `after JULY`: neither (both start in JUNE).
//!
//! The truth table itself is engine-tested; these pin the MCP wiring, the
//! predicate-replaces-open_only behavior, and the mutual-exclusion errors.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

/// 2026-06-01T00:00:00Z in Unix ms.
const JUNE: i64 = 1_780_300_800_000;
/// 2026-07-01T00:00:00Z in Unix ms.
const JULY: i64 = 1_782_864_000_000;
/// 2026-08-01T00:00:00Z in Unix ms.
const AUG: i64 = 1_785_542_400_000;

/// Spawn a server (in `dir`) and build the two-edge fixture above. Returns
/// `(server, a_id, x_id, y_id, edge_ax, edge_ay)`.
fn setup_interval_fixture(
    dir: &tempfile::TempDir,
) -> (Server, String, String, String, String, String) {
    let db_path = dir.path().join("allen.redb");
    let scope = topodb::ScopeId::new().to_string();

    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);

    let mut entity = |name: &str| -> String {
        server
            .call_tool_ok(
                "create_entity",
                serde_json::json!({ "name": name }),
                DEFAULT_TIMEOUT,
            )
            .get("id")
            .and_then(|v| v.as_str())
            .expect("create_entity should return id")
            .to_string()
    };
    let a_id = entity("node_a");
    let x_id = entity("node_x");
    let y_id = entity("node_y");

    let mut link = |to: &str| -> String {
        server
            .call_tool_ok(
                "link",
                serde_json::json!({
                    "from_id": a_id,
                    "to_id": to,
                    "edge_type": "relates_to",
                    "valid_from": JUNE
                }),
                DEFAULT_TIMEOUT,
            )
            .get("id")
            .and_then(|v| v.as_str())
            .expect("link should return id")
            .to_string()
    };
    let edge_ax = link(&x_id);
    let edge_ay = link(&y_id);
    server.call_tool_ok(
        "close_edge",
        serde_json::json!({ "id": edge_ay, "valid_to": JULY }),
        DEFAULT_TIMEOUT,
    );

    (server, a_id, x_id, y_id, edge_ax, edge_ay)
}

/// Sorted edge ids from a `get_edges` result.
fn edge_ids(result: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = result["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .filter_map(|e| e["id"].as_str().map(String::from))
        .collect();
    ids.sort();
    ids
}

/// Sorted node ids from a `traverse` subgraph.
fn node_ids(result: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = result["subgraph"]["nodes"]
        .as_array()
        .expect("subgraph nodes array")
        .iter()
        .filter_map(|n| n["id"].as_str().map(String::from))
        .collect();
    ids.sort();
    ids
}

#[test]
fn get_edges_allen_predicates_gate_on_the_valid_interval() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, _x, _y, edge_ax, edge_ay) = setup_interval_fixture(&dir);
    let mut ids_for = |params: serde_json::Value| -> Vec<String> {
        edge_ids(&server.call_tool_ok("get_edges", params, DEFAULT_TIMEOUT))
    };

    // during [JUNE, JULY]: only the closed edge — containment needs a right
    // end, and `valid_to == b` passes the half-open bound. Note the CLOSED
    // edge comes back without `open_only: false`: the predicate REPLACES the
    // open-only default.
    assert_eq!(
        ids_for(serde_json::json!({ "from_id": a_id, "valid_during": [JUNE, JULY] })),
        vec![edge_ay.clone()],
    );

    // overlaps [JUNE, JULY): both — the open edge trivially, the closed one
    // because `valid_to (JULY) > a (JUNE)`.
    let mut both = vec![edge_ax.clone(), edge_ay.clone()];
    both.sort();
    assert_eq!(
        ids_for(serde_json::json!({ "from_id": a_id, "valid_overlaps": [JUNE, JULY] })),
        both,
    );

    // overlaps [JULY, AUG): only the open edge — the closed one ends exactly
    // at `a`, and overlaps requires `valid_to > a`.
    assert_eq!(
        ids_for(serde_json::json!({ "from_id": a_id, "valid_overlaps": [JULY, AUG] })),
        vec![edge_ax.clone()],
    );

    // before JULY: only the closed edge (an open edge is never fully over).
    assert_eq!(
        ids_for(serde_json::json!({ "from_id": a_id, "valid_before": JULY })),
        vec![edge_ay.clone()],
    );

    // after JULY: neither — both started in JUNE.
    assert_eq!(
        ids_for(serde_json::json!({ "from_id": a_id, "valid_after": JULY })),
        Vec::<String>::new(),
    );
}

#[test]
fn get_edges_allen_predicates_reject_conflicts_and_bad_intervals() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, ..) = setup_interval_fixture(&dir);

    for (params, needle) in [
        // Two predicates at once: the error names both offenders.
        (
            serde_json::json!({ "from_id": a_id,
                "valid_during": [JUNE, JULY], "valid_after": JULY }),
            "valid_during and valid_after",
        ),
        // The predicate replaces the as_of/open_only gate — each explicit
        // combination is a named conflict.
        (
            serde_json::json!({ "from_id": a_id,
                "valid_overlaps": [JUNE, JULY], "as_of": JULY }),
            "as_of",
        ),
        (
            serde_json::json!({ "from_id": a_id,
                "valid_overlaps": [JUNE, JULY], "open_only": true }),
            "open_only",
        ),
        // Valid axis only.
        (
            serde_json::json!({ "from_id": a_id,
                "valid_before": JULY, "time_axis": "recorded" }),
            "valid axis",
        ),
        // Inverted interval: the engine's ValidInterval::from_parts rejects.
        (
            serde_json::json!({ "from_id": a_id, "valid_during": [JULY, JUNE] }),
            "range is inverted",
        ),
    ] {
        let resp = server.call_tool("get_edges", params.clone(), DEFAULT_TIMEOUT);
        expect_tool_error(&resp);
        assert!(
            resp.to_string().contains(needle),
            "{needle:?} not named in get_edges error for {params}: {resp}"
        );
    }
}

#[test]
fn traverse_allen_predicates_gate_hops_and_reject_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let (mut server, a_id, x_id, y_id, ..) = setup_interval_fixture(&dir);

    // overlaps [JULY, AUG): only the open A->X hop survives, so the subgraph
    // is {A, X}.
    let overlapping = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "valid_overlaps": [JULY, AUG] }),
        DEFAULT_TIMEOUT,
    );
    let mut expect_ax = vec![a_id.clone(), x_id.clone()];
    expect_ax.sort();
    assert_eq!(node_ids(&overlapping), expect_ax);

    // during [JUNE, JULY]: only the closed A->Y hop qualifies — and it is
    // followed even though it is closed (the predicate replaces the
    // open-edge gate).
    let contained = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "valid_during": [JUNE, JULY] }),
        DEFAULT_TIMEOUT,
    );
    let mut expect_ay = vec![a_id.clone(), y_id.clone()];
    expect_ay.sort();
    assert_eq!(node_ids(&contained), expect_ay);

    // Composition rules mirror get_edges (traverse has no open_only).
    for (params, needle) in [
        (
            serde_json::json!({ "seed_id": a_id,
                "valid_overlaps": [JUNE, JULY], "as_of": JULY }),
            "as_of",
        ),
        (
            serde_json::json!({ "seed_id": a_id,
                "valid_before": JULY, "time_axis": "recorded" }),
            "valid axis",
        ),
        (
            serde_json::json!({ "seed_id": a_id,
                "valid_before": JULY, "valid_after": JUNE }),
            "valid_before and valid_after",
        ),
    ] {
        let resp = server.call_tool("traverse", params.clone(), DEFAULT_TIMEOUT);
        expect_tool_error(&resp);
        assert!(
            resp.to_string().contains(needle),
            "{needle:?} not named in traverse error for {params}: {resp}"
        );
    }
}
