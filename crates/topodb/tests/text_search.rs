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
        ..Default::default()
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
            ..Default::default()
        },
    ] {
        assert!(matches!(
            db.search_text_with(&scopes, "anything", 10, &opts),
            Err(TopoError::Rejected(_))
        ));
    }
}

/// Analyzer v1: morphological variants land on one posting — the biggest
/// lexical blind spot of the old exact-token tokenizer.
#[test]
fn stemming_matches_morphological_variants() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = memory("the database is running smoothly", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();

    for query in ["databases", "database", "run", "runs", "smooth"] {
        let hits = db.search_text(&scopes, query, 10).unwrap();
        assert_eq!(hits.len(), 1, "query {query:?} must match via stemming");
        assert_eq!(hits[0].0.id, a);
    }
}

/// Analyzer v1: camelCase identifiers split into their words (snake_case
/// already splits at the non-alphanumeric boundary), acronym-aware.
#[test]
fn camel_case_identifiers_are_searchable_by_word() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = memory("refactored parseHttpRequest into HTTPServer", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();

    for query in ["parse", "http", "request", "server", "parseHttpRequest"] {
        let hits = db.search_text(&scopes, query, 10).unwrap();
        assert_eq!(hits.len(), 1, "query {query:?} must match via camel split");
        assert_eq!(hits[0].0.id, a);
    }
}

/// Miss-only fuzzy fallback: a typo'd or prefix-truncated query term that
/// matches nothing expands to its nearest vocabulary neighbors at a
/// discount; a term that hits exactly pays nothing and scores identically.
#[test]
fn fuzzy_fallback_recovers_typos_and_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (a, op_a) = memory("vector recall pipeline design", Scope::Id(s));
    db.submit(vec![op_a]).unwrap();

    // Typo: one transposition ("vectro" -> "vector").
    let hits = db.search_text(&scopes, "vectro", 10).unwrap();
    assert_eq!(hits.len(), 1, "typo must recover via edit distance");
    assert_eq!(hits[0].0.id, a);

    // Prefix: "pipel" is a prefix of "pipelin" (the stem of "pipeline").
    let hits = db.search_text(&scopes, "pipel", 10).unwrap();
    assert_eq!(hits.len(), 1, "prefix must recover");

    // The fuzzy contribution is discounted below an exact hit's.
    let exact = db.search_text(&scopes, "vector", 10).unwrap()[0].1;
    let fuzzy = db.search_text(&scopes, "vectro", 10).unwrap()[0].1;
    assert!(
        fuzzy < exact,
        "fuzzy score {fuzzy} must be below exact score {exact}"
    );

    // Garbage stays garbage: nothing within distance 2 of "zzzzzzz".
    assert!(db.search_text(&scopes, "zzzzzzz", 10).unwrap().is_empty());

    // Opt-out: with the fallback disabled the typo matches nothing.
    let opts = SearchOptions {
        fuzzy_fallback: false,
        ..Default::default()
    };
    assert!(db
        .search_text_with(&scopes, "vectro", 10, &opts)
        .unwrap()
        .is_empty());
}

/// The v5 analyzer-versioning contract: FTS tables written under a different
/// (or pre-stamp) analyzer are drained and rebuilt on open — simulated by
/// clearing the stamp and the postings from outside, exactly what a pre-v5
/// file looks like to this build.
#[test]
fn stale_analyzer_version_triggers_fts_rebuild_on_open() {
    use redb::TableDefinition;
    const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
    const POSTINGS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("postings");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let s = ScopeId::new();
    let a;
    {
        let db = Db::open_with(&path, spec()).unwrap();
        let (id, op) = memory("reindex probe for analyzers", Scope::Id(s));
        a = id;
        db.submit(vec![op]).unwrap();
    }
    {
        let raw = redb::Database::open(&path).unwrap();
        let tx = raw.begin_write().unwrap();
        {
            let mut postings = tx.open_table(POSTINGS).unwrap();
            postings.retain(|_, _| false).unwrap();
            let mut meta = tx.open_table(META).unwrap();
            meta.insert("fts_analyzer_version", 0u32.to_le_bytes().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let db = Db::open_with(&path, spec()).unwrap();
    let hits = db.search_text(&ScopeSet::of(&[s]), "analyzer", 10).unwrap();
    assert_eq!(hits.len(), 1, "stale analyzer stamp must force a rebuild");
    assert_eq!(hits[0].0.id, a);
}

fn superseded_memory(content: &str, scope: Scope, superseded_at: i64) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    props.insert("superseded_at".into(), PropValue::Int(superseded_at));
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

/// The liveness filter must run BEFORE top-k truncation (a dead hit must not
/// consume the k window), and the tombstone is a timestamp: a mark in the
/// query's future keeps the node (same semantics as `recall`'s
/// `tombstone_prop`). Ranking premise: `dead` matches all three query terms,
/// `future` two, `live` one — strictly descending BM25.
#[test]
fn search_text_live_drops_tombstoned_hits_before_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (dead, op_dead) = superseded_memory("alpha beta gamma", Scope::Id(s), 1_000);
    let (future, op_future) = superseded_memory("alpha beta", Scope::Id(s), 3_000);
    let (live, op_live) = memory("alpha", Scope::Id(s));
    db.submit(vec![op_dead, op_future, op_live]).unwrap();

    // Raw search still sees everything; `dead` outranks all (premise check).
    let raw = db.search_text(&scopes, "alpha beta gamma", 1).unwrap();
    assert_eq!(raw[0].0.id, dead);

    // now = 2000 sits between the two marks: `dead` (1000) is gone, `future`
    // (3000) survives — and fills the k=1 window `dead` would have eaten.
    let now2000 = SearchOptions {
        now_ms: Some(2_000),
        ..SearchOptions::default()
    };
    let hits = db
        .search_text_live(&scopes, "alpha beta gamma", 1, &now2000, &["superseded_at"])
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, future);

    let hits = db
        .search_text_live(&scopes, "alpha beta gamma", 3, &now2000, &["superseded_at"])
        .unwrap();
    assert_eq!(
        hits.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
        vec![future, live]
    );

    // Default options read the wall clock: both millisecond-era marks are in
    // the past, so only the unmarked node survives.
    let hits = db
        .search_text_live(
            &scopes,
            "alpha beta gamma",
            3,
            &SearchOptions::default(),
            &["superseded_at"],
        )
        .unwrap();
    assert_eq!(
        hits.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
        vec![live]
    );
}

/// A non-Int value under the tombstone prop is not a tombstone — the node is
/// kept (mirrors `recall`, which only drops `PropValue::Int` marks).
#[test]
fn search_text_live_keeps_nodes_with_non_int_tombstone_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str("delta epsilon".into()));
    props.insert("superseded_at".into(), PropValue::Str("not-a-mark".into()));
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "Memory".into(),
        props,
    }])
    .unwrap();

    let hits = db
        .search_text_live(
            &ScopeSet::of(&[s]),
            "delta",
            10,
            &SearchOptions::default(),
            &["superseded_at"],
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.id, id);
}

/// The tombstone set: a node is dropped when ANY listed prop marks it dead.
/// Two nodes, each dead under a DIFFERENT prop, both filtered in one call;
/// the second prop alone must also work (order-independence).
#[test]
fn search_text_live_drops_nodes_tombstoned_by_any_listed_prop() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let mk = |content: &str, prop: Option<(&str, i64)>| {
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(content.into()));
        if let Some((k, ts)) = prop {
            props.insert(k.into(), PropValue::Int(ts));
        }
        (
            id,
            Op::CreateNode {
                id,
                scope: Scope::Id(s),
                label: "Memory".into(),
                props,
            },
        )
    };
    let (_sup, op_a) = mk("kappa lambda", Some(("superseded_at", 1_000)));
    let (_forg, op_b) = mk("kappa lambda", Some(("forgotten_at", 1_000)));
    let (live, op_c) = mk("kappa", None);
    db.submit(vec![op_a, op_b, op_c]).unwrap();

    let both = db
        .search_text_live(
            &scopes,
            "kappa",
            10,
            &SearchOptions::default(),
            &["superseded_at", "forgotten_at"],
        )
        .unwrap();
    assert_eq!(
        both.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
        vec![live],
        "either tombstone prop must drop its node"
    );

    // Only the second prop listed: the superseded node comes back, the
    // forgotten one stays dead — per-prop filtering, not first-prop-only.
    let only_forgotten = db
        .search_text_live(
            &scopes,
            "kappa",
            10,
            &SearchOptions::default(),
            &["forgotten_at"],
        )
        .unwrap();
    assert_eq!(only_forgotten.len(), 2);
    assert!(only_forgotten
        .iter()
        .all(|(n, _)| !n.props.contains_key("forgotten_at")));
}

/// SearchOptions.prop_retain: a mechanism-only string-prop allowlist.
/// A candidate survives iff its `prop` value (missing/non-Str reads as
/// `absent_as` when set) is in `any_of` — filtered BEFORE top-k, so a
/// dropped hit never consumes the result window.
#[test]
fn search_text_prop_retain_filters_by_value_with_absent_default() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let mk = |content: &str, kind: Option<&str>| {
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(content.into()));
        if let Some(k) = kind {
            props.insert("kind".into(), PropValue::Str(k.into()));
        }
        (
            id,
            Op::CreateNode {
                id,
                scope: Scope::Id(s),
                label: "Memory".into(),
                props,
            },
        )
    };
    let (episodic, op_a) = mk("sigma tau", Some("episodic"));
    let (procedural, op_b) = mk("sigma tau", Some("procedural"));
    let (absent, op_c) = mk("sigma tau", None);
    db.submit(vec![op_a, op_b, op_c]).unwrap();

    let retain = |any_of: &[&str], absent_as: Option<&str>| SearchOptions {
        prop_retain: Some(PropRetain {
            prop: "kind".into(),
            any_of: any_of.iter().map(|s| s.to_string()).collect(),
            absent_as: absent_as.map(String::from),
        }),
        ..SearchOptions::default()
    };

    // absent_as maps the missing prop into the vocabulary: only the
    // absent node counts as "semantic".
    let semantic = db
        .search_text_with(
            &scopes,
            "sigma",
            10,
            &retain(&["semantic"], Some("semantic")),
        )
        .unwrap();
    assert_eq!(
        semantic.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
        vec![absent]
    );

    // A multi-value allowlist admits each listed value.
    let both = db
        .search_text_with(
            &scopes,
            "sigma",
            10,
            &retain(&["episodic", "procedural"], Some("semantic")),
        )
        .unwrap();
    let mut got: Vec<_> = both.iter().map(|(n, _)| n.id).collect();
    got.sort();
    let mut want = vec![episodic, procedural];
    want.sort();
    assert_eq!(got, want);

    // No absent_as: a node without the prop never matches.
    let strict = db
        .search_text_with(&scopes, "sigma", 10, &retain(&["semantic"], None))
        .unwrap();
    assert!(
        strict.is_empty(),
        "absent prop must not match without absent_as"
    );
}

/// prop_retain filters BEFORE top-k: with k=1 and the best BM25 hit
/// filtered out, the surviving node still fills the window.
#[test]
fn search_text_prop_retain_filters_before_topk() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let mut strong = Props::new();
    // Repeats the term so BM25 ranks this node first.
    strong.insert(
        "content".into(),
        PropValue::Str("upsilon upsilon upsilon".into()),
    );
    strong.insert("kind".into(), PropValue::Str("episodic".into()));
    let strong_id = NodeId::new();
    let mut weak = Props::new();
    weak.insert("content".into(), PropValue::Str("upsilon phi".into()));
    let weak_id = NodeId::new();
    db.submit(vec![
        Op::CreateNode {
            id: strong_id,
            scope: Scope::Id(s),
            label: "Memory".into(),
            props: strong,
        },
        Op::CreateNode {
            id: weak_id,
            scope: Scope::Id(s),
            label: "Memory".into(),
            props: weak,
        },
    ])
    .unwrap();

    let options = SearchOptions {
        prop_retain: Some(PropRetain {
            prop: "kind".into(),
            any_of: vec!["semantic".into()],
            absent_as: Some("semantic".into()),
        }),
        ..SearchOptions::default()
    };
    let hits = db
        .search_text_with(&scopes, "upsilon", 1, &options)
        .unwrap();
    assert_eq!(
        hits.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
        vec![weak_id],
        "the filtered strong hit must not consume the k=1 window"
    );
}

/// Validation: empty prop / empty any_of are caller bugs, rejected loudly
/// (mirrors the labels empty-allowlist rejection).
#[test]
fn search_text_prop_retain_rejects_empty_prop_and_empty_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    for (prop, any_of) in [("", vec!["semantic".to_string()]), ("kind", vec![])] {
        let options = SearchOptions {
            prop_retain: Some(PropRetain {
                prop: prop.into(),
                any_of,
                absent_as: None,
            }),
            ..SearchOptions::default()
        };
        match db.search_text_with(&scopes, "anything", 10, &options) {
            Err(TopoError::Rejected(_)) => {}
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
