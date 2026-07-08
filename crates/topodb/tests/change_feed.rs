use topodb::*;

#[test]
fn subscriber_receives_applied_ops_with_monotonic_seq() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let rx = db.subscribe(16);
    let scope = Scope::Id(ScopeId::new());
    let (a, b) = (NodeId::new(), NodeId::new());
    db.submit(vec![
        Op::CreateNode { id: a, scope, label: "M".into(), props: Default::default() },
        Op::CreateNode { id: b, scope, label: "M".into(), props: Default::default() },
    ]).unwrap();

    let e1 = rx.recv().unwrap();
    let e2 = rx.recv().unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    assert!(matches!(e1.op.as_ref(), Op::CreateNode { id, .. } if *id == a));
}

#[test]
fn rejected_batches_and_reads_produce_no_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let rx = db.subscribe(16);
    // Rejected batch: close a nonexistent edge.
    assert!(db.submit(vec![Op::CloseEdge { id: EdgeId::new(), valid_to: None }]).is_err());
    // A read:
    let _ = db.nodes_by_label(&ScopeSet::of(&[ScopeId::new()]), "M");
    assert!(rx.try_recv().is_err(), "no events for rejected batches or reads");
}

#[test]
fn ops_since_replays_the_log_and_covers_dropped_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let rx = db.subscribe(1); // tiny buffer — the second event will be dropped
    let scope = Scope::Id(ScopeId::new());
    db.submit(vec![
        Op::CreateNode { id: NodeId::new(), scope, label: "M".into(), props: Default::default() },
        Op::CreateNode { id: NodeId::new(), scope, label: "M".into(), props: Default::default() },
        Op::CreateNode { id: NodeId::new(), scope, label: "M".into(), props: Default::default() },
    ]).unwrap();

    let first = rx.recv().unwrap();
    assert_eq!(first.seq, 1);
    // Buffer of 1 → seqs 2..=3 were dropped. Recover:
    let replay = db.ops_since(first.seq + 1).unwrap();
    let seqs: Vec<u64> = replay.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![2, 3]);
}

#[test]
fn subscribe_zero_capacity_still_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let rx = db.subscribe(0); // clamped to 1 — must not become a rendezvous channel
    db.submit(vec![Op::CreateNode {
        id: NodeId::new(), scope: Scope::Id(ScopeId::new()),
        label: "M".into(), props: Default::default(),
    }]).unwrap();
    assert_eq!(rx.recv().unwrap().seq, 1);
}

#[test]
fn unsupported_format_version_errors_at_open() {
    use redb::{Database, TableDefinition};
    const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    { let _ = Db::open(path.clone()).unwrap(); } // create a valid v1 file
    { // sabotage the version from outside
        let raw = Database::open(&path).unwrap();
        let tx = raw.begin_write().unwrap();
        { let mut t = tx.open_table(META).unwrap();
          t.insert("format_version", 999u32.to_le_bytes().as_slice()).unwrap(); }
        tx.commit().unwrap();
    }
    match Db::open(path) {
        Err(TopoError::UnsupportedFormat { found: 999, supported: 1 }) => {}
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
}
