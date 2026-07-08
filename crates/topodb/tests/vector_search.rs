use topodb::*;

fn db_with(nodes: &[(NodeId, Scope, &[f32])]) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    for (id, scope, v) in nodes {
        db.submit(vec![Op::CreateNode { id: *id, scope: *scope, label: "M".into(), props: Default::default() }]).unwrap();
        db.submit(vec![Op::SetEmbedding { id: *id, model: "m1".into(), vector: v.to_vec() }]).unwrap();
    }
    (dir, db)
}

#[test]
fn cosine_ranks_and_respects_scope_and_model() {
    let s1 = ScopeId::new();
    let s2 = ScopeId::new();
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let (_d, db) = db_with(&[
        (a, Scope::Id(s1), &[1.0, 0.0]),
        (b, Scope::Id(s1), &[0.0, 1.0]),
        (c, Scope::Id(s2), &[1.0, 0.0]), // right vector, wrong scope
    ]);
    let hits = db.search_vector(&VectorQuery {
        scopes: ScopeSet::of(&[s1]), model: "m1".into(),
        vector: vec![1.0, 0.0], k: 10, candidates: None,
    }).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.id, a);
    assert!(hits[0].1 > hits[1].1);
    // Unknown model: no hits, not an error.
    assert!(db.search_vector(&VectorQuery {
        scopes: ScopeSet::of(&[s1]), model: "nope".into(),
        vector: vec![1.0, 0.0], k: 10, candidates: None,
    }).unwrap().is_empty());
}

#[test]
fn candidates_restrict_and_supersede_and_remove_apply() {
    let s = ScopeId::new();
    let (a, b) = (NodeId::new(), NodeId::new());
    let (_d, db) = db_with(&[(a, Scope::Id(s), &[1.0, 0.0]), (b, Scope::Id(s), &[1.0, 0.0])]);
    let q = |cands: Option<Vec<NodeId>>| VectorQuery {
        scopes: ScopeSet::of(&[s]), model: "m1".into(),
        vector: vec![1.0, 0.0], k: 10, candidates: cands,
    };
    assert_eq!(db.search_vector(&q(Some(vec![a]))).unwrap().len(), 1);

    // Supersede a's embedding with an orthogonal one — old vector must not score.
    db.submit(vec![Op::SetEmbedding { id: a, model: "m1".into(), vector: vec![0.0, 1.0] }]).unwrap();
    let hits = db.search_vector(&q(None)).unwrap();
    assert_eq!(hits[0].0.id, b);
    assert!(hits.iter().find(|(n, _)| n.id == a).unwrap().1 < 0.01);

    db.submit(vec![Op::RemoveNode { id: b }]).unwrap();
    assert!(db.search_vector(&q(None)).unwrap().iter().all(|(n, _)| n.id != b));
}

#[test]
fn dim_mismatch_rejects_whole_batch_atomically() {
    let s = ScopeId::new();
    let a = NodeId::new();
    let (_d, db) = db_with(&[(a, Scope::Id(s), &[1.0, 0.0])]);
    let b = NodeId::new();
    let err = db.submit(vec![
        Op::CreateNode { id: b, scope: Scope::Id(s), label: "M".into(), props: Default::default() },
        Op::SetEmbedding { id: b, model: "m1".into(), vector: vec![1.0, 0.0, 0.0] }, // dim 3 vs slab dim 2
    ]).unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)));
    // Atomic: the CreateNode in the same batch must not have landed.
    assert!(db.node(&ScopeSet::of(&[s]), b).is_none());
}

#[test]
fn slabs_survive_rebuild_and_reopen() {
    let s = ScopeId::new();
    let a = NodeId::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        db.submit(vec![Op::CreateNode { id: a, scope: Scope::Id(s), label: "M".into(), props: Default::default() }]).unwrap();
        db.submit(vec![Op::SetEmbedding { id: a, model: "m1".into(), vector: vec![1.0, 0.0] }]).unwrap();
        db.rebuild_state_from_ops().unwrap();
        assert_eq!(db.search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s]), model: "m1".into(),
            vector: vec![1.0, 0.0], k: 1, candidates: None,
        }).unwrap().len(), 1);
    }
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    assert_eq!(db.search_vector(&VectorQuery {
        scopes: ScopeSet::of(&[s]), model: "m1".into(),
        vector: vec![1.0, 0.0], k: 1, candidates: None,
    }).unwrap().len(), 1);
}
