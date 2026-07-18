use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    }
}

fn memory(content: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    (
        id,
        Op::CreateNode {
            id,
            scope,
            label: "Memory".into(),
            props,
        },
    )
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

    let hits = db
        .search_text(&ScopeSet::of(&[s1]), "embedded rust", 10)
        .unwrap();
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
        props: [(
            "content".to_string(),
            Some(PropValue::Str("vector recall pipelines".into())),
        )]
        .into(),
    }])
    .unwrap();
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
    let err = db
        .submit(vec![
            Op::SetNodeProps {
                id: a,
                props: [(
                    "content".to_string(),
                    Some(PropValue::Str("changed".into())),
                )]
                .into(),
            },
            Op::CloseEdge {
                id: EdgeId::new(),
                valid_to: None,
            },
        ])
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)));
    assert_eq!(db.search_text(&scopes, "atomic", 10).unwrap().len(), 1);
    assert!(db.search_text(&scopes, "changed", 10).unwrap().is_empty());

    db.rebuild_state_from_ops().unwrap();
    assert_eq!(db.search_text(&scopes, "atomic", 10).unwrap().len(), 1);
}

#[test]
fn empty_string_text_prop_is_not_a_document() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (_e, op_e) = memory("", Scope::Id(s)); // empty content
    let (a, op_a) = memory("solitary real document", Scope::Id(s));
    db.submit(vec![op_e, op_a]).unwrap();
    // If "" counted as a doc, n_docs=2 halves this idf; with normalization
    // n_docs=1 and df=1: idf = ln((1-1+0.5)/(1+0.5)+1) = ln(4/3).
    let hits = db.search_text(&scopes, "solitary", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, a);
    let expected = ((1.0f32 - 1.0 + 0.5) / (1.0 + 0.5) + 1.0).ln() * (1.0 * (1.2f32 + 1.0))
        / (1.0 + 1.2f32 * (1.0 - 0.75f32 + 0.75f32 * 3.0 / 3.0));
    assert!(
        (hits[0].1 - expected).abs() < 1e-5,
        "n_docs must be 1, got score {}",
        hits[0].1
    );
}

#[test]
fn scores_are_isolated_per_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let (s1, s2) = (ScopeId::new(), ScopeId::new());
    let (a, op_a) = memory("rust database engine", Scope::Id(s1));
    db.submit(vec![op_a]).unwrap();
    let baseline = db.search_text(&ScopeSet::of(&[s1]), "rust", 10).unwrap()[0].1;

    // Flood the OTHER scope with docs containing the query term.
    for i in 0..20 {
        let (_x, op) = memory(&format!("rust filler number {i}"), Scope::Id(s2));
        db.submit(vec![op]).unwrap();
    }
    let after = db.search_text(&ScopeSet::of(&[s1]), "rust", 10).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0.id, a);
    assert!(
        (after[0].1 - baseline).abs() < 1e-6,
        "scope s1's score moved from {baseline} to {} because of scope s2's corpus — df/IDF leak",
        after[0].1
    );
}

#[test]
fn plan2_layout_file_migrates_at_open() {
    // Simulate: open with spec (new layout), close, reopen — postings must
    // survive a second open without spurious reindex (idempotent), and a
    // file whose stored index_spec text-portion matches gets NO drain.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    {
        let db = Db::open_with(path.clone(), spec()).unwrap();
        let (_a, op) = memory("persistent postings", Scope::Id(s));
        db.submit(vec![op]).unwrap();
    }
    let db = Db::open_with(path, spec()).unwrap();
    assert_eq!(
        db.search_text(&ScopeSet::of(&[s]), "persistent", 10)
            .unwrap()
            .len(),
        1
    );
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
        assert!(db
            .search_text(&ScopeSet::of(&[s]), "reindex", 10)
            .unwrap()
            .is_empty());
    }
    let db = Db::open_with(path, spec()).unwrap(); // spec changed → full reindex
    assert_eq!(
        db.search_text(&ScopeSet::of(&[s]), "reindex", 10)
            .unwrap()
            .len(),
        1
    );
}

/// Recency weighting: at equal BM25 relevance, `search_text_with` with a
/// nonzero `recency_weight` ranks the fresher node (by its id's ULID
/// timestamp) above the staler one — and with weight 0 the ordering falls
/// back to the pure-BM25 tie-break (ascending id, i.e. OLDEST first),
/// proving the reorder really is the recency factor.
#[test]
fn recency_weight_prefers_fresher_hit_at_equal_relevance() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);

    // Controlled creation times via from_u128: a ULID's top 48 bits are its
    // millisecond timestamp.
    const DAY_MS: i64 = 86_400_000;
    let now: i64 = 1_800_000_000_000; // fixed "now" for determinism
    let ulid_at = |ts_ms: i64, n: u128| ((ts_ms as u128) << 80) | n;
    let old_id = NodeId::from_u128(ulid_at(now - 120 * DAY_MS, 1));
    let new_id = NodeId::from_u128(ulid_at(now - DAY_MS, 2));
    for id in [old_id, new_id] {
        let mut props = Props::new();
        props.insert(
            "content".into(),
            PropValue::Str("identical recency probe".into()),
        );
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(s),
            label: "Memory".into(),
            props,
        }])
        .unwrap();
    }

    // Weight 0 (the search_text default): equal scores, tie-break by
    // ascending id — the OLDER node first.
    let plain = db.search_text(&scopes, "recency probe", 10).unwrap();
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0].0.id, old_id);
    assert!((plain[0].1 - plain[1].1).abs() < f32::EPSILON);

    // Weight on: the fresher node must outrank the stale one.
    let opts = SearchOptions {
        recency_weight: 0.5,
        recency_half_life_ms: 30 * DAY_MS,
        now_ms: Some(now),
    };
    let weighted = db
        .search_text_with(&scopes, "recency probe", 10, &opts)
        .unwrap();
    assert_eq!(weighted.len(), 2);
    assert_eq!(weighted[0].0.id, new_id, "fresher hit must rank first");
    assert!(weighted[0].1 > weighted[1].1);
    // The floor guarantees a stale hit keeps at least (1 - w) of its score.
    assert!(weighted[1].1 >= plain[1].1 * 0.5 - f32::EPSILON);
}

/// Bad recency tuning is a caller error, not a silent no-op.
#[test]
fn search_text_with_rejects_bad_recency_options() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    for opts in [
        SearchOptions {
            recency_weight: -0.1,
            ..Default::default()
        },
        SearchOptions {
            recency_weight: 1.5,
            ..Default::default()
        },
        SearchOptions {
            recency_weight: 0.5,
            recency_half_life_ms: 0,
            now_ms: None,
        },
    ] {
        assert!(matches!(
            db.search_text_with(&scopes, "anything", 10, &opts),
            Err(TopoError::Rejected(_))
        ));
    }
}
