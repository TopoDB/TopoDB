use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![],
    }
}

fn entity(name: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("name".into(), PropValue::Str(name.into()));
    (
        id,
        Op::CreateNode {
            id,
            scope,
            label: "Entity".into(),
            props,
        },
    )
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
        .nodes_by_prop(
            &ScopeSet::of(&[s1]),
            "Entity",
            "name",
            &PropValue::Str("ada".into()),
        )
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
        .nodes_by_prop(
            &ScopeSet::of(&[s1]),
            "Entity",
            "name",
            &PropValue::Float(1.0),
        )
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
    }])
    .unwrap();
    assert!(db
        .nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("ada".into()))
        .unwrap()
        .is_empty());
    assert_eq!(
        db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into()))
            .unwrap()
            .len(),
        1
    );

    // None-removal: clearing the declared prop deletes the index entry while
    // the node itself survives.
    db.submit(vec![Op::SetNodeProps {
        id: a,
        props: [("name".to_string(), None)].into(),
    }])
    .unwrap();
    assert!(db
        .nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into()))
        .unwrap()
        .is_empty());
    assert!(
        db.node(&scopes, a).is_some(),
        "node must survive a prop clear"
    );

    // Re-set so the remove below exercises removal of an indexed node.
    db.submit(vec![Op::SetNodeProps {
        id: a,
        props: [("name".to_string(), Some(PropValue::Str("grace".into())))].into(),
    }])
    .unwrap();
    assert_eq!(
        db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into()))
            .unwrap()
            .len(),
        1
    );

    // Remove: gone from the index.
    db.submit(vec![Op::RemoveNode { id: a }]).unwrap();
    assert!(db
        .nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into()))
        .unwrap()
        .is_empty());
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
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(s),
            label: "Memory".into(),
            props,
        }])
        .unwrap();
    }
    let hits = db.nodes_by_float_range(&ScopeSet::of(&[s]), "importance", 0.0, 0.4);
    assert_eq!(hits.len(), 1);
    // Nothing without the scope:
    assert!(db
        .nodes_by_float_range(&ScopeSet::of(&[ScopeId::new()]), "importance", 0.0, 1.0)
        .is_empty());
}

/// C1 regression: a `(label, prop)` pair declared for the FIRST time on an
/// open that finds pre-existing nodes must have those nodes reindexed, not
/// silently missed. In v2 this was free (`graph.rs` rebuilt the equality
/// index in RAM on every open); v3's PROP_INDEX is an on-disk table
/// maintained incrementally, so `ensure_index_spec` must notice the
/// equality-list change and rebuild it from NODES, exactly like it already
/// does for the text list.
#[test]
fn newly_declared_equality_index_finds_preexisting_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    let a;
    {
        // Opened with NO equality declaration: "ada" is written but never
        // indexed.
        let db = Db::open_with(&path, IndexSpec::default()).unwrap();
        let (id, op) = entity("ada", Scope::Id(s));
        a = id;
        db.submit(vec![op]).unwrap();
    }
    // Reopen declaring the equality index for the first time.
    let db = Db::open_with(&path, spec()).unwrap();
    let hits = db
        .nodes_by_prop(
            &ScopeSet::of(&[s]),
            "Entity",
            "name",
            &PropValue::Str("ada".into()),
        )
        .unwrap();
    assert_eq!(hits.len(), 1, "pre-existing node must be reindexed");
    assert_eq!(hits[0].id, a);
}

/// C1 regression: removing a `(label, prop)` equality declaration, mutating
/// the property while the declaration is absent, then re-declaring it must
/// NOT resurrect the stale PROP_INDEX row from before the removal — the
/// re-declare reindex has to reflect current node state, not whatever was on
/// disk from the last time the pair was declared.
#[test]
fn redeclaring_equality_index_does_not_resurrect_stale_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let a;
    {
        // Declared: index "ada" under (Entity, name).
        let db = Db::open_with(&path, spec()).unwrap();
        let (id, op) = entity("ada", Scope::Id(s));
        a = id;
        db.submit(vec![op]).unwrap();
    }
    {
        // Reopen WITHOUT the declaration, then change the value while it's
        // undeclared — no index maintenance happens for this prop while the
        // declaration is absent.
        let db = Db::open_with(&path, IndexSpec::default()).unwrap();
        db.submit(vec![Op::SetNodeProps {
            id: a,
            props: [("name".to_string(), Some(PropValue::Str("grace".into())))].into(),
        }])
        .unwrap();
    }
    // Re-declare the same (label, prop) pair.
    let db = Db::open_with(&path, spec()).unwrap();
    assert!(
        db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("ada".into()))
            .unwrap()
            .is_empty(),
        "stale row for the old value must not resurface"
    );
    let hits = db
        .nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("grace".into()))
        .unwrap();
    assert_eq!(hits.len(), 1, "current value must be indexed on redeclare");
    assert_eq!(hits[0].id, a);
}

/// The dedup primitive: `nodes_by_prop_normalized` matches Str values case-
/// and whitespace-insensitively, while `nodes_by_prop` stays byte-exact via
/// its post-filter — both against the same (normalized) on-disk keys.
#[test]
fn normalized_lookup_matches_case_and_whitespace_variants() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = entity("Drew Powell", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();

    for variant in [
        "drew powell",
        "DREW POWELL",
        " Drew  Powell ",
        "Drew\u{a0}Powell",
    ] {
        let hits = db
            .nodes_by_prop_normalized(&scopes, "Entity", "name", &PropValue::Str(variant.into()))
            .unwrap();
        assert_eq!(hits.len(), 1, "variant {variant:?} must match");
        assert_eq!(hits[0].id, a);
    }

    // Exact lookup only matches the stored bytes.
    assert!(db
        .nodes_by_prop(
            &scopes,
            "Entity",
            "name",
            &PropValue::Str("drew powell".into())
        )
        .unwrap()
        .is_empty());
    assert_eq!(
        db.nodes_by_prop(
            &scopes,
            "Entity",
            "name",
            &PropValue::Str("Drew Powell".into())
        )
        .unwrap()
        .len(),
        1
    );

    // A genuinely different name matches neither way.
    assert!(db
        .nodes_by_prop_normalized(&scopes, "Entity", "name", &PropValue::Str("Drew".into()))
        .unwrap()
        .is_empty());
}

/// Two nodes whose names differ only in case share one normalized index key:
/// exact lookup separates them, normalized lookup returns both.
#[test]
fn exact_lookup_separates_case_variant_twins() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = entity("Ada", Scope::Id(s));
    let (b, op_b) = entity("ada", Scope::Id(s));
    db.submit(vec![op_a, op_b]).unwrap();

    let exact = db
        .nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("Ada".into()))
        .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].id, a);

    let mut both: Vec<NodeId> = db
        .nodes_by_prop_normalized(&scopes, "Entity", "name", &PropValue::Str("ADA".into()))
        .unwrap()
        .into_iter()
        .map(|n| n.id)
        .collect();
    both.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(both, expected);
}

/// The v5 upgrade path: a file whose PROP_INDEX predates key normalization
/// (simulated by draining the table and clearing the norm-version stamp via
/// raw redb) must be rebuilt on the next open — the `prop_index_norm_version`
/// check in `ensure_index_spec`, not the spec comparison, is what triggers
/// it, since the spec itself is unchanged.
#[test]
fn stale_norm_version_triggers_prop_index_rebuild_on_open() {
    use redb::TableDefinition;
    const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
    const PROP_INDEX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("prop_index");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    let a;
    {
        let db = Db::open_with(&path, spec()).unwrap();
        let (id, op) = entity("Drew Powell", Scope::Id(s));
        a = id;
        db.submit(vec![op]).unwrap();
    }
    {
        // Sabotage from outside: empty the index and mark its key scheme as
        // version 0 (what a pre-v5 file effectively is).
        let raw = redb::Database::open(&path).unwrap();
        let tx = raw.begin_write().unwrap();
        {
            let mut prop_index = tx.open_table(PROP_INDEX).unwrap();
            prop_index.retain(|_, _| false).unwrap();
            let mut meta = tx.open_table(META).unwrap();
            meta.insert("prop_index_norm_version", 0u32.to_le_bytes().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let db = Db::open_with(&path, spec()).unwrap();
    let hits = db
        .nodes_by_prop_normalized(
            &ScopeSet::of(&[s]),
            "Entity",
            "name",
            &PropValue::Str("drew powell".into()),
        )
        .unwrap();
    assert_eq!(hits.len(), 1, "stale norm version must force a rebuild");
    assert_eq!(hits[0].id, a);
}

#[test]
fn open_with_rejects_float_equality_declaration_and_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let bad = IndexSpec {
        equality: vec![
            PropIndex {
                label: "M".into(),
                prop: "x".into(),
            },
            PropIndex {
                label: "M".into(),
                prop: "x".into(),
            },
        ],
        text: vec![],
    };
    assert!(matches!(
        Db::open_with(dir.path().join("t.redb"), bad),
        Err(TopoError::Rejected(_))
    ));
}
