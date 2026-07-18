//! Behavioral tests for Db::recall — the production hybrid fusion API.
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

fn text_only(scopes: &ScopeSet, query: &str, k: usize) -> RecallQuery {
    RecallQuery {
        scopes: scopes.clone(),
        query: query.into(),
        k,
        vector: None,
        expansions: vec![],
        graph_boost: false,
        options: SearchOptions::default(),
    }
}

#[test]
fn text_only_recall_orders_like_search_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (_a, op_a) = memory("rust embedded database engine", Scope::Id(s));
    let (_b, op_b) = memory("rust gardening tips", Scope::Id(s));
    let (_c, op_c) = memory("cooking with rust free pans", Scope::Id(s));
    db.submit(vec![op_a, op_b, op_c]).unwrap();

    let bm25: Vec<NodeId> = db
        .search_text(&scopes, "rust database", 10)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    let fused: Vec<NodeId> = db
        .recall(&text_only(&scopes, "rust database", 10))
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert_eq!(fused, bm25, "single-leg recall must preserve BM25 order");
}

#[test]
fn recall_truncates_to_k_and_validates_input() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    for i in 0..5 {
        let (_x, op) = memory(&format!("common token filler {i}"), Scope::Id(s));
        db.submit(vec![op]).unwrap();
    }
    assert_eq!(
        db.recall(&text_only(&scopes, "common", 2)).unwrap().len(),
        2
    );

    // k == 0 and token-less query reject exactly like search_text.
    assert!(matches!(
        db.recall(&text_only(&scopes, "common", 0)),
        Err(TopoError::Rejected(_))
    ));
    assert!(matches!(
        db.recall(&text_only(&scopes, "!!!", 10)),
        Err(TopoError::Rejected(_))
    ));
    // Empty query vector is a host bug — loud, not a silent skipped leg.
    let mut q = text_only(&scopes, "common", 5);
    q.vector = Some(("m".into(), vec![]));
    assert!(matches!(db.recall(&q), Err(TopoError::Rejected(_))));
}

#[test]
fn recall_rejects_bad_recency_options_despite_leg_zeroing() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (_a, op) = memory("validation probe", Scope::Id(s));
    db.submit(vec![op]).unwrap();

    let mut q = text_only(&scopes, "probe", 5);
    q.options.recency_weight = 1.5;
    assert!(matches!(db.recall(&q), Err(TopoError::Rejected(_))));

    let mut q2 = text_only(&scopes, "probe", 5);
    q2.options.recency_weight = 0.5;
    q2.options.recency_half_life_ms = 0;
    assert!(matches!(db.recall(&q2), Err(TopoError::Rejected(_))));
}

#[test]
fn vector_leg_surfaces_semantic_hit_and_agreement_wins() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    // A: lexical match only. B: vector match only. C: both (agreement).
    let (a, op_a) = memory("login password rotation policy", Scope::Id(s));
    let (b, op_b) = memory("credential storage decision", Scope::Id(s));
    let (c, op_c) = memory("login credentials audit", Scope::Id(s));
    db.submit(vec![op_a, op_b, op_c]).unwrap();
    // Hand-built 2d embeddings: query points at [1,0].
    db.submit(vec![
        Op::SetEmbedding {
            id: a,
            model: "m".into(),
            vector: vec![0.0, 1.0],
        },
        Op::SetEmbedding {
            id: b,
            model: "m".into(),
            vector: vec![0.9, 0.1],
        },
        Op::SetEmbedding {
            id: c,
            model: "m".into(),
            vector: vec![1.0, 0.0],
        },
    ])
    .unwrap();

    let mut q = text_only(&scopes, "login", 10);
    q.vector = Some(("m".into(), vec![1.0, 0.0]));
    let hits: Vec<NodeId> = db
        .recall(&q)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();

    assert_eq!(hits[0], c, "text+vector agreement must rank first");
    assert!(
        hits.contains(&b),
        "vector-only hit must surface despite zero token overlap"
    );

    // Unknown model = empty leg, not an error; pure text order remains.
    let mut q2 = text_only(&scopes, "login", 10);
    q2.vector = Some(("nonexistent-model".into(), vec![1.0, 0.0]));
    let hits2 = db.recall(&q2).unwrap();
    assert!(hits2.iter().all(|(n, _)| n.id == a || n.id == c));
}

#[test]
fn graph_boost_surfaces_linked_but_lexically_silent_neighbor() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (hit, op_h) = memory("deployment pipeline broke on friday", Scope::Id(s));
    // Linked context that shares NO tokens with the query:
    let (linked, op_l) = memory("rollback procedure: revert then redeploy", Scope::Id(s));
    let (_stray, op_s) = memory("unrelated grocery list", Scope::Id(s));
    db.submit(vec![op_h, op_l, op_s]).unwrap();
    db.submit(vec![Op::CreateEdge {
        id: EdgeId::new(),
        scope: Scope::Id(s),
        ty: "about".into(),
        from: linked,
        to: hit,
        props: Props::new(),
        valid_from: None,
    }])
    .unwrap();

    let mut q = text_only(&scopes, "deployment friday", 10);
    q.graph_boost = true;
    let ids: Vec<NodeId> = db
        .recall(&q)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert_eq!(ids[0], hit, "direct text hit stays first");
    assert!(
        ids.contains(&linked),
        "1-hop neighbor must join the results"
    );
    assert!(!ids.contains(&_stray), "unlinked, unmatched node stays out");

    // graph_boost=false: neighbor absent.
    let q2 = text_only(&scopes, "deployment friday", 10);
    let ids2: Vec<NodeId> = db
        .recall(&q2)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert!(!ids2.contains(&linked));
}

#[test]
fn recency_applies_once_post_fusion() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    const DAY_MS: i64 = 86_400_000;
    let now: i64 = 1_800_000_000_000;
    let ulid_at = |ts: i64, n: u128| ((ts as u128) << 80) | n;
    let old_id = NodeId::from_u128(ulid_at(now - 120 * DAY_MS, 1));
    let new_id = NodeId::from_u128(ulid_at(now - DAY_MS, 2));
    for id in [old_id, new_id] {
        let mut props = Props::new();
        props.insert(
            "content".into(),
            PropValue::Str("identical fusion probe".into()),
        );
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(s),
            label: "Memory".into(),
            props,
        }])
        .unwrap();
    }
    let mut q = text_only(&scopes, "fusion probe", 10);
    q.options = SearchOptions {
        recency_weight: 0.5,
        recency_half_life_ms: 30 * DAY_MS,
        now_ms: Some(now),
        ..Default::default()
    };
    let hits = db.recall(&q).unwrap();
    assert_eq!(
        hits[0].0.id, new_id,
        "fresher node must rank first post-fusion"
    );
    assert!(hits[0].1 > hits[1].1);
}

#[test]
fn expansions_surface_synonym_hits_at_a_discount() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (exact, op_e) = memory("auth flow redesign notes", Scope::Id(s));
    let (syn, op_s) = memory("login page rework details", Scope::Id(s));
    db.submit(vec![op_e, op_s]).unwrap();

    // Without expansions: "auth" finds only the exact memory.
    let plain: Vec<NodeId> = db
        .recall(&text_only(&scopes, "auth", 10))
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert_eq!(plain, vec![exact]);

    // With host-resolved expansion auth->login: both surface, exact first.
    let mut q = text_only(&scopes, "auth", 10);
    q.expansions = vec![("auth".into(), vec!["login".into()])];
    let hits = db.recall(&q).unwrap();
    let ids: Vec<NodeId> = hits.iter().map(|(n, _)| n.id).collect();
    assert!(ids.contains(&exact) && ids.contains(&syn));
    assert_eq!(
        ids[0], exact,
        "exact term hit must outrank the discounted expansion"
    );
}

#[test]
fn discounted_contributions_never_stack_past_one_discount() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (syn, op_s) = memory("login page rework details", Scope::Id(s));
    let (other, op_o) = memory("deploy pipeline caching notes", Scope::Id(s));
    db.submit(vec![op_s, op_o]).unwrap();

    // One expansion entry: baseline discounted score for the synonym hit.
    let mut q1 = text_only(&scopes, "auth deploy", 10);
    q1.expansions = vec![("auth".into(), vec!["login".into()])];
    let hits1 = db.recall(&q1).unwrap();
    let syn_score_1 = hits1.iter().find(|(n, _)| n.id == syn).unwrap().1;

    // Duplicate query word -> two identical expansion entries (what the MCP
    // layer produces for "auth auth deploy"): the discounted contribution
    // must NOT double.
    let mut q2 = text_only(&scopes, "auth auth deploy", 10);
    q2.expansions = vec![
        ("auth".into(), vec!["login".into()]),
        ("auth".into(), vec!["login".into()]),
    ];
    let hits2 = db.recall(&q2).unwrap();
    let syn_score_2 = hits2.iter().find(|(n, _)| n.id == syn).unwrap().1;
    assert!(
        (syn_score_2 - syn_score_1).abs() < 1e-5,
        "duplicate expansion entries must not stack: {syn_score_1} vs {syn_score_2}"
    );
    let _ = other;
}

#[test]
fn expansion_token_matching_exact_hit_does_not_re_add() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (m, op) = memory("login flow design", Scope::Id(s));
    db.submit(vec![op]).unwrap();

    // "login" hits exactly; a synonym auth->login must not add a second,
    // discounted helping of the same token to the same doc.
    let plain = db.recall(&text_only(&scopes, "login auth", 10)).unwrap();
    let base = plain.iter().find(|(n, _)| n.id == m).unwrap().1;
    let mut q = text_only(&scopes, "login auth", 10);
    q.expansions = vec![("auth".into(), vec!["login".into()])];
    let hits = db.recall(&q).unwrap();
    let with_exp = hits.iter().find(|(n, _)| n.id == m).unwrap().1;
    assert!(
        (with_exp - base).abs() < 1e-5,
        "expansion equal to an exact-hit term must be a no-op: {base} vs {with_exp}"
    );
}
