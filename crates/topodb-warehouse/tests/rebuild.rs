use topodb::{Db, NodeId, Op, PropValue, Scope};
use topodb_warehouse::{rebuild, Warehouse, WarehouseConfig};

#[test]
fn rebuild_reproduces_op_log_and_graph_from_segments() {
    let t = tempfile::tempdir().unwrap();
    let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
    let mut wh = Warehouse::open(&t.path().join("w"), WarehouseConfig::default()).unwrap();
    // a mixed history: nodes, an embedding, an edge, a close, a prop change, a removal
    let scope = Scope::Shared;
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let mut p = topodb::Props::new();
    p.insert("content".into(), PropValue::Str("A".into()));
    db.submit_at(
        vec![Op::CreateNode {
            id: a,
            scope,
            label: "Memory".into(),
            props: p.clone(),
        }],
        1_000,
    )
    .unwrap();
    db.submit_at(
        vec![
            Op::CreateNode {
                id: b,
                scope,
                label: "Memory".into(),
                props: p.clone(),
            },
            Op::CreateNode {
                id: c,
                scope,
                label: "Entity".into(),
                props: p,
            },
        ],
        2_000,
    )
    .unwrap();
    db.submit_at(
        vec![Op::SetEmbedding {
            id: a,
            model: "m".into(),
            vector: vec![1.0, 2.0, 3.0],
        }],
        3_000,
    )
    .unwrap();
    let e = topodb::EdgeId::new();
    db.submit_at(
        vec![Op::CreateEdge {
            id: e,
            scope,
            ty: "about".into(),
            from: a,
            to: c,
            props: Default::default(),
            valid_from: None,
            recorded_at: None,
        }],
        4_000,
    )
    .unwrap();
    let mut ch = std::collections::BTreeMap::new();
    ch.insert("content".to_string(), Some(PropValue::Str("A2".into())));
    db.submit_at(vec![Op::SetNodeProps { id: a, props: ch }], 5_000)
        .unwrap();
    db.submit_at(
        vec![Op::CloseEdge {
            id: e,
            valid_to: None,
            superseded_at: None,
        }],
        6_000,
    )
    .unwrap();
    db.submit_at(vec![Op::RemoveNode { id: b }], 7_000).unwrap();
    wh.mirror(&db, 8_000).unwrap();
    // seal + one more op after sealing, mirrored into a fresh open segment
    topodb_warehouse::segment::seal_open(&wh.layout, &mut wh.manifest).unwrap();
    wh.save().unwrap();
    db.submit_at(vec![Op::RemoveNode { id: c }], 9_000).unwrap();
    wh.mirror(&db, 10_000).unwrap();

    let out = t.path().join("rebuilt.redb");
    let rep = rebuild(&wh, &out, topodb_json::default_spec()).unwrap();
    assert_eq!(
        (rep.first_seq, rep.last_seq, rep.gaps.len()),
        (1, db.current_seq().unwrap(), 0)
    );
    let db2 = Db::open_with(&out, topodb_json::default_spec()).unwrap();
    let orig: Vec<Op> = db
        .ops_since(1)
        .unwrap()
        .into_iter()
        .map(|c| (*c.op).clone())
        .collect();
    let rebuilt: Vec<Op> = db2
        .ops_since(1)
        .unwrap()
        .into_iter()
        .map(|c| (*c.op).clone())
        .collect();
    assert_eq!(orig, rebuilt);
    let set = topodb_json::scope_to_scope_set(Scope::Shared);
    let n1 = db.node(&set, a).unwrap();
    let n2 = db2.node(&set, a).unwrap();
    assert_eq!((n1.props, n1.embedding), (n2.props, n2.embedding));
    assert!(db2.node(&set, b).is_none() && db2.node(&set, c).is_none());
}
