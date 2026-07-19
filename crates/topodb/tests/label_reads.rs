//! Public-behavior regression net for the index-driven label reads (F9-11
//! Task 8): `Db::nodes_by_label_newest`'s newest-first, k-bounded contract —
//! the `recent_memories` shape — plus scope filtering and the empty/k=0
//! edge cases. `Db::nodes_by_label`'s set/order behavior is covered by the
//! differential oracle (`tests/differential.rs`) and the format fixtures
//! (`tests/format_fixture.rs`); this file is specifically about the NEW
//! `nodes_by_label_newest` entry point.
use std::{thread, time::Duration};
use topodb::{Db, NodeId, Op, Scope, ScopeId, ScopeSet};

fn create(db: &Db, scope: Scope, label: &str) -> NodeId {
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id,
        scope,
        label: label.into(),
        props: Default::default(),
    }])
    .unwrap();
    id
}

/// Three same-label nodes created in strict succession, staggered so their
/// ULIDs are guaranteed distinct in mint-time order (ULID timestamp
/// resolution is 1ms): `k=2` must return exactly the newest two, newest
/// first — not the oldest two, not some order-independent set.
#[test]
fn newest_k_returns_newest_first_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let scope_id = ScopeId::new();
    let scope = Scope::Id(scope_id);

    let oldest = create(&db, scope, "Memory");
    thread::sleep(Duration::from_millis(2));
    let middle = create(&db, scope, "Memory");
    thread::sleep(Duration::from_millis(2));
    let newest = create(&db, scope, "Memory");

    let scopes = ScopeSet::of(&[scope_id]);
    let top2 = db.nodes_by_label_newest(&scopes, "Memory", 2);
    assert_eq!(
        top2.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![newest, middle],
        "k=2 must return the newest two, newest-first"
    );

    let all3 = db.nodes_by_label_newest(&scopes, "Memory", 3);
    assert_eq!(
        all3.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![newest, middle, oldest],
        "k >= corpus size must return every match, still newest-first"
    );

    let over = db.nodes_by_label_newest(&scopes, "Memory", 100);
    assert_eq!(
        over.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![newest, middle, oldest],
        "k larger than the corpus must not error or pad — just return everything, newest-first"
    );
}

/// A node outside `scopes` must never surface, however new it is: scope
/// filtering applies BEFORE the newest-k cut, not after (an implementation
/// that took the engine-wide newest k across all scopes and only then
/// filtered by scope could silently under-fill or wrongly empty the result
/// for a caller with a narrow `ScopeSet`).
#[test]
fn newest_k_respects_scope_filter() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let mine = ScopeId::new();
    let theirs = ScopeId::new();

    let my_old = create(&db, Scope::Id(mine), "Memory");
    thread::sleep(Duration::from_millis(2));
    // Newer in wall-clock terms, but in a scope the caller doesn't read.
    let _their_new = create(&db, Scope::Id(theirs), "Memory");

    let scopes = ScopeSet::of(&[mine]);
    let hits = db.nodes_by_label_newest(&scopes, "Memory", 5);
    assert_eq!(
        hits.iter().map(|n| n.id).collect::<Vec<_>>(),
        vec![my_old],
        "a newer out-of-scope node must never displace an older in-scope one"
    );
}

/// A label that was never created (never interned, so it has no
/// `LABEL_INDEX` rows at all) must degrade to empty, not error — matching
/// `nodes_by_label`'s "unknown label contributes nothing" contract.
#[test]
fn newest_k_unknown_label_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let scope_id = ScopeId::new();
    create(&db, Scope::Id(scope_id), "Memory");

    let scopes = ScopeSet::of(&[scope_id]);
    let hits = db.nodes_by_label_newest(&scopes, "NoSuchLabel", 5);
    assert!(
        hits.is_empty(),
        "unknown label must yield no hits, not an error"
    );
}

/// `k = 0` is pinned to "empty", matching `nodes_by_label`'s convention of
/// degrading rather than rejecting (there is no `k` on `nodes_by_label` to
/// directly mirror, so this picks the same "degrade to empty" shape its
/// storage-read-failure and unknown-label/scope paths already use, rather
/// than surfacing a `Result::Err`/panic for a technically-valid k=0 call).
#[test]
fn newest_k_zero_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let scope_id = ScopeId::new();
    create(&db, Scope::Id(scope_id), "Memory");

    let scopes = ScopeSet::of(&[scope_id]);
    let hits = db.nodes_by_label_newest(&scopes, "Memory", 0);
    assert!(
        hits.is_empty(),
        "k=0 must yield no hits, not an error or a panic"
    );
}
