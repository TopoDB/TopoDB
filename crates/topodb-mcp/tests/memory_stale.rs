//! `find_stale_memories`: surface memories that have gone cold — not created or
//! recalled within `older_than_days`, stalest first. "Activity" is the later of
//! the memory's mint time (ULID) and its last recall (`last_accessed_at`), so a
//! brand-new memory is never stale and a frequently-recalled one stays fresh.
//! Read-only and advisory. Crucially the scan must NOT itself count as a recall
//! (it reads the very recency signal a normal read would bump), so it uses a
//! non-bumping enumeration. Superseded memories are excluded. Pure graph/counter
//! op, no embeddings — the whole suite runs in CI.

mod common;

use common::{Server, DEFAULT_TIMEOUT as T};
use serde_json::json;
use std::time::{Duration, Instant};

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

fn fresh() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(T);
    (dir, s)
}

fn mem(s: &mut Server, content: &str) -> String {
    s.call_tool_ok("create_memory", json!({ "content": content }), T)["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Recall counters are bumped asynchronously; block until `id` shows at least
/// `min` accesses so assertions are deterministic. access_stats never bumps.
fn wait_access(s: &mut Server, id: &str, min: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let st = s.call_tool_ok("access_stats", json!({ "id": id }), T);
        if st["access_count"].as_u64().unwrap_or(0) >= min {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "access_count never reached {min} for {id}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stale_ids(res: &serde_json::Value) -> Vec<String> {
    res["stale"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn stalest_first_and_a_recall_freshens() {
    let (_d, mut s) = fresh();
    let a = mem(&mut s, "alpha fact");
    let _b = mem(&mut s, "bravo fact");
    let c = mem(&mut s, "charlie fact");

    // ms-granularity timestamps: force the recall into a later millisecond than
    // charlie's mint, or stalest-first ties and the order is tiebreak-dependent
    // (flaked on fast CI runners).
    std::thread::sleep(Duration::from_millis(5));

    // Recall bravo -> its last_accessed_at jumps to now, the freshest of the three.
    let b = _b;
    s.call_tool_ok("get_node", json!({ "id": b }), T);
    wait_access(&mut s, &b, 1);

    // older_than_days: 0 -> every memory qualifies; order is stalest-first.
    let res = s.call_tool_ok("find_stale_memories", json!({ "older_than_days": 0 }), T);
    let ids = stale_ids(&res);
    assert_eq!(ids.len(), 3, "all three qualify at threshold 0: {res}");
    assert_eq!(
        ids.last().unwrap(),
        &b,
        "the recalled memory is least stale: {res}"
    );
    // a and c were never recalled, so both are staler than the just-recalled b.
    let pos = |id: &String| ids.iter().position(|x| x == id).unwrap();
    assert!(
        pos(&a) < pos(&b) && pos(&c) < pos(&b),
        "unrecalled sort ahead of recalled: {res}"
    );

    let row = |id: &str| {
        res["stale"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone()
    };
    assert!(row(&b)["access_count"].as_u64().unwrap() >= 1, "{res}");
    assert!(
        !row(&b)["last_accessed_at"].is_null(),
        "recalled => timestamp present: {res}"
    );
    assert_eq!(row(&a)["access_count"], 0, "{res}");
    assert!(
        row(&a)["last_accessed_at"].is_null(),
        "never recalled => null: {res}"
    );
}

#[test]
fn freshly_created_memories_are_not_stale() {
    let (_d, mut s) = fresh();
    mem(&mut s, "one");
    mem(&mut s, "two");
    // Everything was created seconds ago; nothing is older than 10 years.
    let res = s.call_tool_ok("find_stale_memories", json!({ "older_than_days": 3650 }), T);
    assert_eq!(
        res["stale"].as_array().unwrap().len(),
        0,
        "nothing is that old: {res}"
    );
    assert_eq!(res["scanned"], 2, "both live memories examined: {res}");
}

#[test]
fn superseded_memories_are_not_stale() {
    let (_d, mut s) = fresh();
    let old = mem(&mut s, "old fact");
    s.call_tool_ok(
        "remember",
        json!({ "content": "corrected fact", "entities": ["Topic"], "supersedes": [old] }),
        T,
    );
    let res = s.call_tool_ok("find_stale_memories", json!({ "older_than_days": 0 }), T);
    assert!(
        !stale_ids(&res).contains(&old),
        "retired memory is not stale: {res}"
    );
    assert_eq!(
        res["scanned"], 1,
        "only the live replacement is scanned: {res}"
    );
}

#[test]
fn the_scan_does_not_count_as_a_recall() {
    let (_d, mut s) = fresh();
    let untouched = mem(&mut s, "never read");
    let fence = mem(&mut s, "fence");

    // Scan several times. If the scan bumped, `untouched` would gain accesses.
    for _ in 0..3 {
        s.call_tool_ok("find_stale_memories", json!({ "older_than_days": 0 }), T);
    }
    // Fence: recall `fence` AFTER the scans, then wait for its bump. The applier
    // is FIFO, so once the fence's (later) bump lands, any scan-induced bumps
    // would have landed too.
    s.call_tool_ok("get_node", json!({ "id": fence }), T);
    wait_access(&mut s, &fence, 1);

    let st = s.call_tool_ok("access_stats", json!({ "id": untouched }), T);
    assert_eq!(
        st["access_count"], 0,
        "a maintenance scan must not register as a recall: {st}"
    );
}

#[test]
fn empty_and_bounds() {
    let (_d, mut s) = fresh();
    let res = s.call_tool_ok("find_stale_memories", json!({}), T);
    assert_eq!(res["stale"].as_array().unwrap().len(), 0, "{res}");
    assert_eq!(res["scanned"], 0, "{res}");
    assert_eq!(res["truncated"], false, "{res}");

    common::expect_tool_error(&s.call_tool("find_stale_memories", json!({ "limit": 0 }), T));
    common::expect_tool_error(&s.call_tool("find_stale_memories", json!({ "limit": 1001 }), T));
    common::expect_tool_error(&s.call_tool(
        "find_stale_memories",
        json!({ "older_than_days": -1 }),
        T,
    ));
}
