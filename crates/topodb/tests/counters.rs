use std::time::{Duration, Instant};
use topodb::*;

fn wait_for_count(db: &Db, scopes: &ScopeSet, id: NodeId, want_at_least: u64) -> AccessStats {
    // Bumps are async (batched ~100ms). Poll with a deadline instead of sleeping blind.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(stats) = db.access_stats(scopes, id).unwrap() {
            if stats.access_count >= want_at_least {
                return stats;
            }
        }
        assert!(
            Instant::now() < deadline,
            "counter never reached {want_at_least}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn reads_bump_counters_asynchronously() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();

    assert_eq!(
        db.access_stats(&scopes, id).unwrap(),
        Some(AccessStats::default())
    );
    let _ = db.node(&scopes, id);
    let _ = db.node(&scopes, id);
    let stats = wait_for_count(&db, &scopes, id, 2);
    assert!(stats.last_accessed_at > 0);
}

#[test]
fn nodes_by_label_unbumped_reads_without_bumping() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let untouched = NodeId::new();
    let fence = NodeId::new();
    for id in [untouched, fence] {
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(s),
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
    }

    // The unbumped scan returns the whole label population...
    let hits = db.nodes_by_label_unbumped(&scopes, "M");
    assert_eq!(hits.len(), 2, "sees both nodes");

    // ...but must not have bumped anything. Fence: bump `fence` AFTER the scan;
    // once its (later) bump lands, any scan-induced bump would have landed too.
    let _ = db.node(&scopes, fence);
    wait_for_count(&db, &scopes, fence, 1);
    assert_eq!(
        db.access_stats(&scopes, untouched).unwrap(),
        Some(AccessStats::default()),
        "an unbumped scan must leave the access counters untouched"
    );
}

#[test]
fn counters_are_outside_log_feed_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let rx = db.subscribe(16);
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();
    let _ = rx.recv().unwrap(); // consume the CreateNode event

    let _ = db.node(&scopes, id);
    let stats = wait_for_count(&db, &scopes, id, 1);

    // 1. No feed events from bumps:
    assert!(rx.try_recv().is_err());
    // 2. No ops in the log beyond the create:
    assert_eq!(db.ops_since(1).unwrap().len(), 1);
    // 3. Rebuild preserves counters (they are not derived from the log):
    db.rebuild_state_from_ops().unwrap();
    assert_eq!(db.access_stats(&scopes, id).unwrap(), Some(stats));
}

// NOTE: the I1 counter-identity-across-rebuild regression lives in
// `src/migrate_v3.rs`'s test mod
// (`rebuild_after_migration_keeps_counters_with_their_ulid_when_slots_diverge`),
// not here. The slot divergence it guards against only exists on a MIGRATED
// v2 file — migration assigns slots in ULID-iteration order while replay
// assigns them in op order; a pure-v3 database replays every node back to
// its identical slot, so any test built here passes even against a rebuild
// that never touches COUNTERS at all. Building a v2 file requires the
// crate-private frozen v2 encoders, hence the unit-test location.

#[test]
fn stats_respect_scope_and_reads_of_stats_do_not_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();

    assert_eq!(
        db.access_stats(&ScopeSet::of(&[ScopeId::new()]), id)
            .unwrap(),
        None
    );
    for _ in 0..5 {
        let _ = db.access_stats(&scopes, id).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        db.access_stats(&scopes, id).unwrap(),
        Some(AccessStats::default())
    );
}

#[test]
fn nodes_by_prop_bumps_results() {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![PropIndex {
            label: "M".into(),
            prop: "k".into(),
        }],
        text: vec![],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("k".into(), PropValue::Str("x".into()));
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props,
    }])
    .unwrap();

    let hits = db
        .nodes_by_prop(&scopes, "M", "k", &PropValue::Str("x".into()))
        .unwrap();
    assert_eq!(hits.len(), 1);
    let stats = wait_for_count(&db, &scopes, id, 1);
    assert!(stats.access_count >= 1);
}

#[test]
fn removed_node_stats_are_none_despite_orphan_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();
    let _ = db.node(&scopes, id);
    let _ = wait_for_count(&db, &scopes, id, 1); // orphan row now exists in COUNTERS
    db.submit(vec![Op::RemoveNode { id }]).unwrap();
    assert_eq!(
        db.access_stats(&scopes, id).unwrap(),
        None,
        "gate on node existence, not row existence"
    );
}

#[test]
fn multi_hit_prop_lookup_bumps_every_returned_node() {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![PropIndex {
            label: "M".into(),
            prop: "k".into(),
        }],
        text: vec![],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let ids: Vec<NodeId> = (0..3).map(|_| NodeId::new()).collect();
    for id in &ids {
        let mut props = Props::new();
        props.insert("k".into(), PropValue::Str("same".into()));
        db.submit(vec![Op::CreateNode {
            id: *id,
            scope: Scope::Id(s),
            label: "M".into(),
            props,
        }])
        .unwrap();
    }
    assert_eq!(
        db.nodes_by_prop(&scopes, "M", "k", &PropValue::Str("same".into()))
            .unwrap()
            .len(),
        3
    );
    for id in &ids {
        let stats = wait_for_count(&db, &scopes, *id, 1);
        assert!(
            stats.access_count >= 1,
            "every hit bumps, not just the first"
        );
    }
}

#[test]
fn rejected_prop_lookup_does_not_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap(); // empty spec — every lookup Rejected
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();
    assert!(db
        .nodes_by_prop(&scopes, "M", "k", &PropValue::Int(1))
        .is_err());
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        db.access_stats(&scopes, id).unwrap(),
        Some(AccessStats::default())
    );
}

#[test]
fn float_range_scan_does_not_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("importance".into(), PropValue::Float(0.5));
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "M".into(),
        props,
    }])
    .unwrap();

    assert_eq!(
        db.nodes_by_float_range(&scopes, "importance", 0.0, 1.0)
            .len(),
        1
    );
    std::thread::sleep(std::time::Duration::from_millis(300)); // > one bumper flush interval
    assert_eq!(
        db.access_stats(&scopes, id).unwrap(),
        Some(AccessStats::default())
    );
}

#[test]
fn search_text_unbumped_leaves_access_counters_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();

    // Create a Memory node with searchable text.
    let mut props = Props::new();
    props.insert(
        "content".into(),
        PropValue::Str("rust database engine architecture".into()),
    );
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "Memory".into(),
        props,
    }])
    .unwrap();

    // Call search_text and verify access count rose.
    let hits = db.search_text(&scopes, "database", 10).unwrap();
    assert_eq!(hits.len(), 1, "search_text must find the node");
    assert_eq!(hits[0].0.id, id);
    let bumped_score = hits[0].1;

    let bumped_stats = wait_for_count(&db, &scopes, id, 1);
    assert!(
        bumped_stats.access_count >= 1,
        "search_text must bump access count"
    );

    // Record the count after the bumped call.
    let count_after_bumped = bumped_stats.access_count;

    // Call search_text_unbumped with the same query.
    let unbumped_hits = db.search_text_unbumped(&scopes, "database", 10).unwrap();

    // Verify the unbumped call returned the same nodes in the same order
    // with the same scores (both use default BM25, only the bump differs).
    assert_eq!(
        unbumped_hits.len(),
        1,
        "search_text_unbumped must find the node"
    );
    assert_eq!(unbumped_hits[0].0.id, id);
    assert_eq!(
        unbumped_hits[0].1, bumped_score,
        "unbumped must return the same score as bumped (same BM25, only bump differs)"
    );

    // Give async bumps time to land, then verify the count did NOT change.
    std::thread::sleep(Duration::from_millis(300));
    let final_stats = db.access_stats(&scopes, id).unwrap();
    assert_eq!(
        final_stats,
        Some(AccessStats {
            access_count: count_after_bumped,
            last_accessed_at: bumped_stats.last_accessed_at,
        }),
        "search_text_unbumped must not bump access counters"
    );
}
