use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex { label: "Entity".into(), prop: "name".into() }],
        text: vec![],
    }
}

fn entity(name: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("name".into(), PropValue::Str(name.into()));
    (id, Op::CreateNode { id, scope, label: "Entity".into(), props })
}

#[test]
fn equality_lookup_finds_only_declared_and_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let (s1, s2) = (ScopeId::new(), ScopeId::new());
    let (a, op_a) = entity("ada", Scope::Id(s1));
    let (_b, op_b) = entity("ada", Scope::Id(s2)); // same name, other scope
    db.submit(vec![op_a, op_b]).unwrap();

    let hits = db
        .nodes_by_prop(&ScopeSet::of(&[s1]), "Entity", "name", &PropValue::Str("ada".into()))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, a);

    // Undeclared key is rejected, not silently empty:
    let err = db
        .nodes_by_prop(&ScopeSet::of(&[s1]), "Entity", "age", &PropValue::Int(1))
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)));

    // Float query value is rejected too, even for a declared key:
    let err = db
        .nodes_by_prop(&ScopeSet::of(&[s1]), "Entity", "name", &PropValue::Float(1.0))
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)));
}

#[test]
fn index_follows_set_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = entity("ada", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();

    // Rename: old key empty, new key hits.
    db.submit(vec![Op::SetNodeProps {
        id: a,
        props: [("name".to_string(), Some(PropValue::Str("grace".into())))].into(),
    }]).unwrap();
    assert!(db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("ada".into())).unwrap().is_empty());
    assert_eq!(db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into())).unwrap().len(), 1);

    // Remove: gone from the index.
    db.submit(vec![Op::RemoveNode { id: a }]).unwrap();
    assert!(db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into())).unwrap().is_empty());
}

#[test]
fn float_range_scan_is_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    for imp in [0.1f64, 0.5, 0.9] {
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert("importance".into(), PropValue::Float(imp));
        db.submit(vec![Op::CreateNode { id, scope: Scope::Id(s), label: "Memory".into(), props }]).unwrap();
    }
    let hits = db.nodes_by_float_range(&ScopeSet::of(&[s]), "importance", 0.0, 0.4);
    assert_eq!(hits.len(), 1);
    // Nothing without the scope:
    assert!(db.nodes_by_float_range(&ScopeSet::of(&[ScopeId::new()]), "importance", 0.0, 1.0).is_empty());
}

#[test]
fn open_with_rejects_float_equality_declaration_and_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let bad = IndexSpec {
        equality: vec![
            PropIndex { label: "M".into(), prop: "x".into() },
            PropIndex { label: "M".into(), prop: "x".into() },
        ],
        text: vec![],
    };
    assert!(matches!(Db::open_with(dir.path().join("t.redb"), bad), Err(TopoError::Rejected(_))));
}
