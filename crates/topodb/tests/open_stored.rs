use topodb::*;

fn custom_spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex {
            label: "Person".into(),
            prop: "handle".into(),
        }],
        text: vec![PropIndex {
            label: "Note".into(),
            prop: "body".into(),
        }],
    }
}

#[test]
fn open_stored_uses_persisted_spec_no_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    let a = NodeId::new();
    {
        // Create with a custom spec; write an equality-indexed node + a text-indexed node.
        let db = Db::open_with(&path, custom_spec()).unwrap();
        let mut p = Props::new();
        p.insert("handle".into(), PropValue::Str("ada".into()));
        db.submit(vec![Op::CreateNode {
            id: a,
            scope: Scope::Id(s),
            label: "Person".into(),
            props: p,
        }])
        .unwrap();
        let mut n = Props::new();
        n.insert("body".into(), PropValue::Str("first program note".into()));
        db.submit(vec![Op::CreateNode {
            id: NodeId::new(),
            scope: Scope::Id(s),
            label: "Note".into(),
            props: n,
        }])
        .unwrap();
    }
    // Reopen WITHOUT passing a spec — must inherit the custom one.
    let db = Db::open_stored(&path).unwrap();
    let scopes = ScopeSet::of(&[s]);
    // Equality index declared correctly (else this would Rejected or be empty):
    let hits = db
        .nodes_by_prop(&scopes, "Person", "handle", &PropValue::Str("ada".into()))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, a);
    // Text index intact (no drain/reindex wiped it):
    assert_eq!(db.search_text(&scopes, "program", 10).unwrap().len(), 1);
}

#[test]
fn open_stored_fresh_file_uses_default_spec() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_stored(dir.path().join("fresh.redb")).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    // Default spec declares no equality index → an undeclared lookup is Rejected, not a panic.
    assert!(db
        .nodes_by_prop(&scopes, "X", "y", &PropValue::Int(1))
        .is_err());
    // And the db is writable:
    db.submit(vec![Op::CreateNode {
        id: NodeId::new(),
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();
    assert_eq!(db.current_seq().unwrap(), 1);
}
