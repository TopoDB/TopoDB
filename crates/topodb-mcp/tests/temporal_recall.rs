//! Protocol-level e2e for temporal recall on `search_memories`:
//! `created_after`/`created_before` filtering, the deterministic
//! `temporal_rewrite` query rewriter, and the `applied_time_filter` echo
//! (spec: docs/superpowers/specs/2026-08-09-semantica-inclusions-design.md,
//! PR 2). Real server, real db, JSON-RPC over stdio — same harness as
//! `e2e.rs`.
//!
//! Date fixtures are fixed far-past/far-future ISO dates rather than
//! computed "yesterday"/"tomorrow": every memory seeded here is minted at
//! test-run time, so it always falls strictly between the fixtures — no
//! clock math, deterministic on any machine at any date.

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};
use serde_json::json;

/// Date-only bounds resolve to UTC day starts (the parser's documented
/// rule); the echo reports the resolved instant in ms since epoch, so the
/// asserts pin the resolved instant, not the input string.
const PAST: &str = "2000-01-01";
const PAST_MS: i64 = 946_684_800_000;
/// 2099-01-01 stays inside the parser's 1970–2099 year window — the
/// far-future fixture cannot be 9999 or 2100.
const FUTURE: &str = "2099-01-01";
const FUTURE_MS: i64 = 4_070_908_800_000;

/// Fresh server on its own db with one seeded memory (minted "now", so it
/// is always inside [PAST, FUTURE)).
fn server_with_memory(dir: &tempfile::TempDir, db: &str, content: &str) -> Server {
    let mut server = Server::spawn(&dir.path().join(db), &[]);
    server.initialize(DEFAULT_TIMEOUT);
    server.call_tool_ok(
        "create_memory",
        json!({ "content": content }),
        DEFAULT_TIMEOUT,
    );
    server
}

fn hit_count(result: &serde_json::Value) -> usize {
    result["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("result should carry a hits array: {result:#?}"))
        .len()
}

#[test]
fn created_range_params_filter_and_are_echoed() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = server_with_memory(
        &dir,
        "params.redb",
        "the redb storage layer uses copy-on-write",
    );

    // Far-future created_after excludes a memory minted now — and the result
    // says so via applied_time_filter with source "params".
    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "redb", "k": 5, "created_after": FUTURE }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        hit_count(&r),
        0,
        "a far-future created_after must exclude everything: {r:#?}"
    );
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("explicit param must be echoed even on 0 hits: {r:#?}"));
    assert_eq!(f["source"], "params", "filter: {f:#?}");
    assert_eq!(
        f["after"],
        json!(FUTURE_MS),
        "a date-only bound resolves to UTC day start: {f:#?}"
    );
    assert!(
        f.get("before").is_none(),
        "no before bound was given: {f:#?}"
    );
    assert!(
        f.get("matched_phrase").is_none(),
        "the params path never sets matched_phrase: {f:#?}"
    );

    // Far-past created_before excludes it from the other side.
    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "redb", "k": 5, "created_before": PAST }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        hit_count(&r),
        0,
        "a far-past created_before must exclude everything: {r:#?}"
    );
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("explicit param must be echoed even on 0 hits: {r:#?}"));
    assert_eq!(f["source"], "params", "filter: {f:#?}");
    assert_eq!(f["before"], json!(PAST_MS), "filter: {f:#?}");
    assert!(f.get("after").is_none(), "no after bound was given: {f:#?}");

    // An enclosing range keeps the memory and echoes BOTH bounds.
    let r = server.call_tool_ok(
        "search_memories",
        json!({
            "query": "redb",
            "k": 5,
            "created_after": PAST,
            "created_before": FUTURE
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        hit_count(&r),
        1,
        "an enclosing range must keep the in-range memory: {r:#?}"
    );
    let content = r["hits"][0]["node"]["props"]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(
        content.contains("redb"),
        "the surviving hit is the seeded memory: {r:#?}"
    );
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("filter echoed on the hit path too: {r:#?}"));
    assert_eq!(f["source"], "params", "filter: {f:#?}");
    assert_eq!(f["after"], json!(PAST_MS), "filter: {f:#?}");
    assert_eq!(f["before"], json!(FUTURE_MS), "filter: {f:#?}");
}

#[test]
fn no_applicable_filter_means_no_applied_time_filter_field() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = server_with_memory(
        &dir,
        "absent.redb",
        "the redb storage layer uses copy-on-write",
    );

    // No params, no temporal phrase: the field is ABSENT (serde skip), not null.
    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "redb", "k": 5 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(hit_count(&r), 1, "plain search still works: {r:#?}");
    assert!(
        r.get("applied_time_filter").is_none(),
        "no filter applied means the key is omitted entirely: {r:#?}"
    );

    // "before" with no parseable date is NOT a temporal phrase — the
    // conservative rewriter must not fire on it (spec: 'before the v8
    // migration' never triggers), even with temporal_rewrite at its
    // default of true.
    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "redb before the v8 migration", "k": 5 }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        hit_count(&r) >= 1,
        "the query still matches on its other terms: {r:#?}"
    );
    assert!(
        r.get("applied_time_filter").is_none(),
        "no parseable date means the rewriter must not fire: {r:#?}"
    );
}

#[test]
fn inverted_range_is_rejected_not_silently_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = server_with_memory(
        &dir,
        "inverted.redb",
        "the redb storage layer uses copy-on-write",
    );

    // after >= before is a caller bug: invalid_params, never a clean 0-hit
    // result (spec: 'empty range is a caller bug, not an empty result').
    let resp = server.call_tool(
        "search_memories",
        json!({
            "query": "redb",
            "k": 5,
            "created_after": FUTURE,
            "created_before": PAST
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

#[test]
fn rewrite_strips_the_phrase_and_applies_the_range() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = server_with_memory(
        &dir,
        "rewrite.redb",
        "the redb storage layer uses copy-on-write",
    );

    // "since <far past>" keeps a memory minted now; the echo names the
    // rewrite and the exact phrase it consumed.
    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "redb since 2000-01-01", "k": 5, "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        hit_count(&r),
        1,
        "the in-range memory survives the rewritten search: {r:#?}"
    );
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("an applied rewrite must be surfaced: {r:#?}"));
    assert_eq!(f["source"], "rewrite", "filter: {f:#?}");
    assert_eq!(f["matched_phrase"], "since 2000-01-01", "filter: {f:#?}");
    assert_eq!(f["after"], json!(PAST_MS), "filter: {f:#?}");
    assert!(
        f.get("before").is_none(),
        "'since' sets only a lower bound: {f:#?}"
    );

    // "before <far past>" drops it: the parsed range really filters, it is
    // not merely echoed.
    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "redb before 2000-01-01", "k": 5, "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(hit_count(&r), 0, "nothing here predates 2000: {r:#?}");
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("the rewrite is surfaced on the 0-hit path too: {r:#?}"));
    assert_eq!(f["source"], "rewrite", "filter: {f:#?}");
    assert_eq!(f["matched_phrase"], "before 2000-01-01", "filter: {f:#?}");
    assert_eq!(f["before"], json!(PAST_MS), "filter: {f:#?}");
    assert!(
        f.get("after").is_none(),
        "'before' sets only an upper bound: {f:#?}"
    );
}

#[test]
fn rewrite_searches_the_residual_not_the_full_string() {
    let dir = tempfile::tempdir().unwrap();
    // The seeded CONTENT contains the date words. A server that searched the
    // full query string would match it on "since"/"2000"/"01"; only a
    // genuinely stripped residual returns nothing. fuzzy: false keeps the
    // nonsense residual from borrowing prefix/typo neighbors.
    let mut server = server_with_memory(
        &dir,
        "residual.redb",
        "migration window since 2000-01-01 approved",
    );

    let r = server.call_tool_ok(
        "search_memories",
        json!({ "query": "xyzzyqux since 2000-01-01", "k": 5, "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        hit_count(&r),
        0,
        "residual 'xyzzyqux' matches nothing — a hit here means the temporal phrase was searched instead of stripped: {r:#?}"
    );
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("the rewrite still ran and must be echoed: {r:#?}"));
    assert_eq!(f["source"], "rewrite", "filter: {f:#?}");

    // Opt-out: with temporal_rewrite false the SAME query is taken literally —
    // the date words are search terms again (they match the seeded content)
    // and no filter is applied or echoed.
    let r = server.call_tool_ok(
        "search_memories",
        json!({
            "query": "xyzzyqux since 2000-01-01",
            "k": 5,
            "fuzzy": false,
            "temporal_rewrite": false
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        hit_count(&r) >= 1,
        "rewrite off: the date words are plain terms and match the content: {r:#?}"
    );
    assert!(
        r.get("applied_time_filter").is_none(),
        "rewrite off: no filter applied, key omitted: {r:#?}"
    );
}

#[test]
fn explicit_params_take_precedence_and_disable_the_rewriter() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = server_with_memory(
        &dir,
        "precedence.redb",
        "the redb storage layer uses copy-on-write",
    );

    // The query carries a parseable phrase, but an explicit bound is present:
    // the rewriter must NOT run. Proof: no after bound appears (the phrase
    // would have contributed one) and matched_phrase is absent.
    let r = server.call_tool_ok(
        "search_memories",
        json!({
            "query": "redb since 2000-01-01",
            "k": 5,
            "fuzzy": false,
            "created_before": FUTURE
        }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        hit_count(&r) >= 1,
        "the memory is inside the explicit bound and 'redb' still matches: {r:#?}"
    );
    let f = r
        .get("applied_time_filter")
        .unwrap_or_else(|| panic!("explicit param must be echoed: {r:#?}"));
    assert_eq!(
        f["source"], "params",
        "explicit bounds win over the rewriter: {f:#?}"
    );
    assert_eq!(f["before"], json!(FUTURE_MS), "filter: {f:#?}");
    assert!(
        f.get("after").is_none(),
        "'since 2000-01-01' must NOT have been parsed into an after bound: {f:#?}"
    );
    assert!(
        f.get("matched_phrase").is_none(),
        "the rewriter did not run, so no phrase was consumed: {f:#?}"
    );
}
