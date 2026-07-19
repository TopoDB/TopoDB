use topodb::*;

fn db_with(nodes: &[(NodeId, Scope, &[f32])]) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    for (id, scope, v) in nodes {
        db.submit(vec![Op::CreateNode {
            id: *id,
            scope: *scope,
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
        db.submit(vec![Op::SetEmbedding {
            id: *id,
            model: "m1".into(),
            vector: v.to_vec(),
        }])
        .unwrap();
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
    let hits = db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s1]),
            model: "m1".into(),
            vector: vec![1.0, 0.0],
            k: 10,
            candidates: None,
        })
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.id, a);
    assert!(hits[0].1 > hits[1].1);
    // Unknown model: no hits, not an error.
    assert!(db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s1]),
            model: "nope".into(),
            vector: vec![1.0, 0.0],
            k: 10,
            candidates: None,
        })
        .unwrap()
        .is_empty());
}

#[test]
fn candidates_restrict_and_supersede_and_remove_apply() {
    let s = ScopeId::new();
    let (a, b) = (NodeId::new(), NodeId::new());
    let (_d, db) = db_with(&[
        (a, Scope::Id(s), &[1.0, 0.0]),
        (b, Scope::Id(s), &[1.0, 0.0]),
    ]);
    let q = |cands: Option<Vec<NodeId>>| VectorQuery {
        scopes: ScopeSet::of(&[s]),
        model: "m1".into(),
        vector: vec![1.0, 0.0],
        k: 10,
        candidates: cands,
    };
    assert_eq!(db.search_vector(&q(Some(vec![a]))).unwrap().len(), 1);

    // Supersede a's embedding with an orthogonal one — old vector must not score.
    db.submit(vec![Op::SetEmbedding {
        id: a,
        model: "m1".into(),
        vector: vec![0.0, 1.0],
    }])
    .unwrap();
    let hits = db.search_vector(&q(None)).unwrap();
    assert_eq!(hits[0].0.id, b);
    assert!(hits.iter().find(|(n, _)| n.id == a).unwrap().1 < 0.01);

    db.submit(vec![Op::RemoveNode { id: b }]).unwrap();
    assert!(db
        .search_vector(&q(None))
        .unwrap()
        .iter()
        .all(|(n, _)| n.id != b));
}

#[test]
fn dim_mismatch_rejects_whole_batch_atomically() {
    let s = ScopeId::new();
    let a = NodeId::new();
    let (_d, db) = db_with(&[(a, Scope::Id(s), &[1.0, 0.0])]);
    let b = NodeId::new();
    let err = db
        .submit(vec![
            Op::CreateNode {
                id: b,
                scope: Scope::Id(s),
                label: "M".into(),
                props: Default::default(),
            },
            Op::SetEmbedding {
                id: b,
                model: "m1".into(),
                vector: vec![1.0, 0.0, 0.0],
            }, // dim 3 vs slab dim 2
        ])
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)));
    // Atomic: the CreateNode in the same batch must not have landed.
    assert!(db.node(&ScopeSet::of(&[s]), b).is_none());
}

#[test]
fn cross_model_switch_tombstones_old_slab() {
    let s = ScopeId::new();
    let a = NodeId::new();
    let (_d, db) = db_with(&[(a, Scope::Id(s), &[1.0, 0.0])]); // model m1
    db.submit(vec![Op::SetEmbedding {
        id: a,
        model: "m2".into(),
        vector: vec![1.0, 0.0],
    }])
    .unwrap();
    let q = |model: &str| VectorQuery {
        scopes: ScopeSet::of(&[s]),
        model: model.into(),
        vector: vec![1.0, 0.0],
        k: 10,
        candidates: None,
    };
    assert!(
        db.search_vector(&q("m1")).unwrap().is_empty(),
        "old model slab must be tombstoned"
    );
    assert_eq!(db.search_vector(&q("m2")).unwrap().len(), 1);
}

/// v4 BEHAVIOR CHANGE (sanctioned — the v4 storage-format design spec's "one
/// deliberate semantics change", implemented by `check_or_pin_dim` /
/// `VECTOR_DIMS`): dimension pinning is now per-model and PERMANENT, not
/// per-`(model, scope)` and re-settable once a slab empties. Before v4, a
/// fully-tombstoned slab (every row removed) could re-dimension on its next
/// `SetEmbedding` (`vector.rs: Slab::upsert`'s empty-slab re-dimension arm)
/// — this test used to name and pin exactly that ("
/// fully_tombstoned_slab_accepts_new_dimension"). As of v4, `vector_dims`
/// pins "m1" at dim 2 on `a`'s embedding below, and that pin survives even
/// after `a` (and with it every live row in the slab) is removed: `b`'s
/// later `SetEmbedding` under "m1" at a DIFFERENT dim is now rejected. This
/// is the same fixture/setup as the old test, with the assertion flipped to
/// match the new contract.
#[test]
fn fully_tombstoned_model_still_rejects_a_new_dimension() {
    let s = ScopeId::new();
    let a = NodeId::new();
    let (_d, db) = db_with(&[(a, Scope::Id(s), &[1.0, 0.0])]); // m1 pinned at dim 2
    db.submit(vec![Op::RemoveNode { id: a }]).unwrap(); // slab now has no live rows
    let b = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id: b,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();
    // dim 3 under "m1" mismatches the dim `a` pinned above — rejected even
    // though the slab that pin came from is now fully tombstoned.
    let err = db
        .submit(vec![Op::SetEmbedding {
            id: b,
            model: "m1".into(),
            vector: vec![1.0, 0.0, 0.0],
        }])
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)), "got {err:?}");

    // The rejected batch committed nothing: "m1" still has no live rows.
    assert!(db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s]),
            model: "m1".into(),
            vector: vec![1.0, 0.0, 0.0],
            k: 10,
            candidates: None,
        })
        .unwrap()
        .is_empty());

    // The ORIGINALLY pinned dim (2) still works — the pin just no longer
    // permits a DIFFERENT dim, even for an emptied slab.
    db.submit(vec![Op::SetEmbedding {
        id: b,
        model: "m1".into(),
        vector: vec![0.0, 1.0],
    }])
    .unwrap();
    let hits = db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s]),
            model: "m1".into(),
            vector: vec![0.0, 1.0],
            k: 10,
            candidates: None,
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, b);
}

#[test]
fn slabs_survive_rebuild_and_reopen() {
    let s = ScopeId::new();
    let a = NodeId::new();
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        db.submit(vec![Op::CreateNode {
            id: a,
            scope: Scope::Id(s),
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
        db.submit(vec![Op::SetEmbedding {
            id: a,
            model: "m1".into(),
            vector: vec![1.0, 0.0],
        }])
        .unwrap();
        db.rebuild_state_from_ops().unwrap();
        assert_eq!(
            db.search_vector(&VectorQuery {
                scopes: ScopeSet::of(&[s]),
                model: "m1".into(),
                vector: vec![1.0, 0.0],
                k: 1,
                candidates: None,
            })
            .unwrap()
            .len(),
            1
        );
    }
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    assert_eq!(
        db.search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s]),
            model: "m1".into(),
            vector: vec![1.0, 0.0],
            k: 1,
            candidates: None,
        })
        .unwrap()
        .len(),
        1
    );
}

/// A zero-dim embedding must be rejected outright. Accepting one used to fix
/// the `(model, scope)` slab's dim at 0, after which every real embedding
/// under that key was permanently rejected as a dim conflict — one empty
/// array bricked the model namespace. Symmetric with `search_vector`, which
/// has always refused an empty query vector.
#[test]
fn empty_embedding_is_rejected_and_does_not_poison_the_slab() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let (a, b) = (NodeId::new(), NodeId::new());

    for id in [a, b] {
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(s),
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
    }

    let err = db
        .submit(vec![Op::SetEmbedding {
            id: a,
            model: "m1".into(),
            vector: vec![],
        }])
        .unwrap_err();
    assert!(
        matches!(err, TopoError::Rejected(_)),
        "empty embedding should be Rejected, got {err:?}"
    );

    // The slab was never created, so a real embedding still works. Before the
    // fix this failed with "dim 3 does not match existing slab dim 0".
    db.submit(vec![Op::SetEmbedding {
        id: b,
        model: "m1".into(),
        vector: vec![1.0, 2.0, 3.0],
    }])
    .unwrap();

    let hits = db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[s]),
            model: "m1".into(),
            vector: vec![1.0, 2.0, 3.0],
            k: 10,
            candidates: None,
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "the real embedding should be searchable");
    assert_eq!(hits[0].0.id, b);
}

/// The same rule must hold inside a batch, where the empty vector rides along
/// with the `CreateNode` that gives the node its scope.
#[test]
fn empty_embedding_in_a_batch_is_rejected_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let a = NodeId::new();

    let err = db
        .submit(vec![
            Op::CreateNode {
                id: a,
                scope: Scope::Id(s),
                label: "M".into(),
                props: Default::default(),
            },
            Op::SetEmbedding {
                id: a,
                model: "m1".into(),
                vector: vec![],
            },
        ])
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)), "got {err:?}");

    // Atomic: the CreateNode must not have landed either.
    assert!(db.node(&ScopeSet::of(&[s]), a).is_none());
}

#[test]
fn set_embedding_rejects_non_finite_components() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let node_id = NodeId::new();

    db.submit(vec![Op::CreateNode {
        id: node_id,
        scope: Scope::Id(s),
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();

    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let res = db.submit(vec![Op::SetEmbedding {
            id: node_id,
            model: "m".into(),
            vector: vec![1.0, bad, 3.0],
        }]);
        assert!(
            matches!(res, Err(TopoError::Rejected(_))),
            "must reject component {bad}"
        );
    }
    // A finite vector still works.
    db.submit(vec![Op::SetEmbedding {
        id: node_id,
        model: "m".into(),
        vector: vec![1.0, 2.0, 3.0],
    }])
    .unwrap();
}
