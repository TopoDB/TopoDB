use topodb::*;
use std::time::{Duration, Instant};

fn wait_for_count(db: &Db, scopes: &ScopeSet, id: NodeId, want_at_least: u64) -> AccessStats {
    // Bumps are async (batched ~100ms). Poll with a deadline instead of sleeping blind.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(stats) = db.access_stats(scopes, id).unwrap() {
            if stats.access_count >= want_at_least { return stats; }
        }
        assert!(Instant::now() < deadline, "counter never reached {want_at_least}");
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
    db.submit(vec![Op::CreateNode { id, scope: Scope::Id(s), label: "M".into(), props: Default::default() }]).unwrap();

    assert_eq!(db.access_stats(&scopes, id).unwrap(), Some(AccessStats::default()));
    let _ = db.node(&scopes, id);
    let _ = db.node(&scopes, id);
    let stats = wait_for_count(&db, &scopes, id, 2);
    assert!(stats.last_accessed_at > 0);
}

#[test]
fn counters_are_outside_log_feed_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let rx = db.subscribe(16);
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode { id, scope: Scope::Id(s), label: "M".into(), props: Default::default() }]).unwrap();
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

#[test]
fn stats_respect_scope_and_reads_of_stats_do_not_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let id = NodeId::new();
    db.submit(vec![Op::CreateNode { id, scope: Scope::Id(s), label: "M".into(), props: Default::default() }]).unwrap();

    assert_eq!(db.access_stats(&ScopeSet::of(&[ScopeId::new()]), id).unwrap(), None);
    for _ in 0..5 { let _ = db.access_stats(&scopes, id).unwrap(); }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(db.access_stats(&scopes, id).unwrap(), Some(AccessStats::default()));
}
