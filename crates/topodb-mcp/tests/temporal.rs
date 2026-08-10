//! Protocol-level tests for `search_memories` created-time filtering.
//! Engine range semantics (backdated ULIDs, boundary inclusivity, k-fill)
//! are pinned in `topodb`'s own suite; this file pins the wire surface:
//! params parse, the filter actually constrains hits, and
//! `applied_time_filter` reports what ran.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT as T};
use serde_json::json;

/// 2001-01-01T00:00:00Z in ms — safely before anything a test run creates.
const PAST_MS: i64 = 978_307_200_000;

fn seeded_server(dir: &tempfile::TempDir, content: &str) -> Server {
    let mut s = Server::spawn(&dir.path().join("temporal.redb"), &[]);
    s.initialize(T);
    s.call_tool_ok("create_memory", json!({ "content": content }), T);
    s
}

#[test]
fn explicit_bounds_filter_hits_and_are_echoed() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "release gating decision for the beta");

    // Everything in this db was created after 2001, so an upper bound in the
    // past excludes every hit — and the result says which filter ran.
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "release gating", "created_before": "2001-01-01" }),
        T,
    );
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(0), "{out:#?}");
    let filter = &out["applied_time_filter"];
    assert_eq!(filter["source"], "params", "{out:#?}");
    assert_eq!(filter["before"], json!(PAST_MS), "{out:#?}");
    assert!(
        filter.get("after").is_none_or(|v| v.is_null()),
        "unset bound must not be reported: {out:#?}"
    );
    assert!(
        filter.get("matched_phrase").is_none_or(|v| v.is_null()),
        "explicit params report no matched_phrase: {out:#?}"
    );

    // A lower bound in the past keeps the hit and echoes `after`.
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "release gating", "created_after": "2001-01-01" }),
        T,
    );
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(1), "{out:#?}");
    assert_eq!(out["applied_time_filter"]["source"], "params", "{out:#?}");
    assert_eq!(
        out["applied_time_filter"]["after"],
        json!(PAST_MS),
        "{out:#?}"
    );
}

#[test]
fn unfiltered_search_omits_applied_time_filter() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "release gating decision for the beta");
    let out = s.call_tool_ok("search_memories", json!({ "query": "release gating" }), T);
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(1), "{out:#?}");
    assert!(
        out.get("applied_time_filter").is_none(),
        "no filter ran — the field must be absent (serde skip): {out:#?}"
    );
}

#[test]
fn inverted_range_is_rejected_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "anything at all");
    // Engine contract: after >= before is a caller bug (Rejected →
    // invalid_params), not a silent zero-hit result.
    let resp = s.call_tool(
        "search_memories",
        json!({
            "query": "anything",
            "created_after": "2098-01-01",
            "created_before": "2001-01-01"
        }),
        T,
    );
    expect_tool_error(&resp);
}

#[test]
fn unparseable_bound_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "anything at all");
    for bad in ["not-a-date", "08/01/2026", ""] {
        let resp = s.call_tool(
            "search_memories",
            json!({ "query": "anything", "created_after": bad }),
            T,
        );
        expect_tool_error(&resp);
    }
}

/// 2099-01-01T00:00:00Z in ms — safely after anything a test run creates,
/// and still inside the parser's 1970–2099 year window (2100+ would not
/// parse at all).
const FUTURE_MS: i64 = 4_070_908_800_000;

#[test]
fn temporal_phrase_is_rewritten_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "chose redb for the storage engine");
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "storage engine before 2099-01-01" }),
        T,
    );
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(1), "{out:#?}");
    let filter = &out["applied_time_filter"];
    assert_eq!(filter["source"], "rewrite", "{out:#?}");
    assert_eq!(filter["before"], json!(FUTURE_MS), "{out:#?}");
    assert_eq!(filter["matched_phrase"], "before 2099-01-01", "{out:#?}");
}

#[test]
fn residual_query_is_what_gets_searched() {
    let dir = tempfile::tempdir().unwrap();
    // The seed CONTAINS the temporal phrase's own tokens, so searching the
    // full query string would match it; only the stripped residual must run.
    let mut s = seeded_server(&dir, "planning meeting before 2099-01-01 kickoff");
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "qqqzzz before 2099-01-01" }),
        T,
    );
    assert_eq!(
        out["hits"].as_array().map(Vec::len),
        Some(0),
        "residual 'qqqzzz' matches nothing — full-string search would have hit: {out:#?}"
    );
    assert_eq!(out["applied_time_filter"]["source"], "rewrite", "{out:#?}");
}

#[test]
fn explicit_params_win_over_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "chose redb for the storage engine");
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "storage before 2099-01-01", "created_after": "2001-01-01" }),
        T,
    );
    // Rewriter must not run: source is "params", no matched_phrase, and the
    // query was searched verbatim (its "storage" term still hits).
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(1), "{out:#?}");
    let filter = &out["applied_time_filter"];
    assert_eq!(filter["source"], "params", "{out:#?}");
    assert_eq!(filter["after"], json!(PAST_MS), "{out:#?}");
    assert!(
        filter.get("matched_phrase").is_none_or(|v| v.is_null()),
        "{out:#?}"
    );
}

#[test]
fn temporal_rewrite_false_searches_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "chose redb for the storage engine");
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "storage before 2099-01-01", "temporal_rewrite": false }),
        T,
    );
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(1), "{out:#?}");
    assert!(
        out.get("applied_time_filter").is_none(),
        "opt-out must leave the query untouched and unfiltered: {out:#?}"
    );
}

#[test]
fn phrase_without_parseable_date_never_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = seeded_server(&dir, "v8 migration decision recorded");
    let out = s.call_tool_ok(
        "search_memories",
        json!({ "query": "decision before the v8 migration" }),
        T,
    );
    assert_eq!(out["hits"].as_array().map(Vec::len), Some(1), "{out:#?}");
    assert!(
        out.get("applied_time_filter").is_none(),
        "'before <no date>' is conservative — no rewrite: {out:#?}"
    );
}
