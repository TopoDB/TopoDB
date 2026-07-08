use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![],
        text: vec![PropIndex { label: "Memory".into(), prop: "content".into() }],
    }
}

fn memory(content: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    (id, Op::CreateNode { id, scope, label: "Memory".into(), props })
}

#[test]
fn bm25_ranks_matches_and_respects_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let (s1, s2) = (ScopeId::new(), ScopeId::new());
    let (a, op_a) = memory("rust embedded database engine", Scope::Id(s1));
    let (_b, op_b) = memory("gardening tips for spring", Scope::Id(s1));
    let (_c, op_c) = memory("rust embedded database engine", Scope::Id(s2)); // wrong scope
    db.submit(vec![op_a, op_b, op_c]).unwrap();

    let hits = db.search_text(&ScopeSet::of(&[s1]), "embedded rust", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, a);
    assert!(hits[0].1 > 0.0);
}

#[test]
fn index_follows_prop_updates_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = memory("topology of memory graphs", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();
    assert_eq!(db.search_text(&scopes, "topology", 10).unwrap().len(), 1);

    db.submit(vec![Op::SetNodeProps {
        id: a,
        props: [("content".to_string(), Some(PropValue::Str("vector recall pipelines".into())))].into(),
    }]).unwrap();
    assert!(db.search_text(&scopes, "topology", 10).unwrap().is_empty());
    assert_eq!(db.search_text(&scopes, "recall", 10).unwrap().len(), 1);

    db.submit(vec![Op::RemoveNode { id: a }]).unwrap();
    assert!(db.search_text(&scopes, "recall", 10).unwrap().is_empty());
}

#[test]
fn rejected_batch_leaves_postings_untouched_and_rebuild_restores_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = memory("atomic transactional postings", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();

    // Batch = text update + invalid op → whole batch rejected, postings unchanged.
    let err = db.submit(vec![
        Op::SetNodeProps { id: a, props: [("content".to_string(), Some(PropValue::Str("changed".into())))].into() },
        Op::CloseEdge { id: EdgeId::new(), valid_to: None },
    ]).unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)));
    assert_eq!(db.search_text(&scopes, "atomic", 10).unwrap().len(), 1);
    assert!(db.search_text(&scopes, "changed", 10).unwrap().is_empty());

    db.rebuild_state_from_ops().unwrap();
    assert_eq!(db.search_text(&scopes, "atomic", 10).unwrap().len(), 1);
}

#[test]
fn changed_text_spec_reindexes_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    let (_a, op_a) = memory("reindex me on spec change", Scope::Id(s));
    {
        let db = Db::open(path.clone()).unwrap(); // no text spec — nothing indexed
        db.submit(vec![op_a]).unwrap();
        assert!(db.search_text(&ScopeSet::of(&[s]), "reindex", 10).unwrap().is_empty());
    }
    let db = Db::open_with(path, spec()).unwrap(); // spec changed → full reindex
    assert_eq!(db.search_text(&ScopeSet::of(&[s]), "reindex", 10).unwrap().len(), 1);
}
