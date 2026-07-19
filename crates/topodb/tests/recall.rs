//! Behavioral tests for Db::recall — the production hybrid fusion API.
use std::time::Duration;
use topodb::*;

/// Bumps are async (batched ~100ms / 256-item flush threshold, see
/// `crates/topodb/tests/counters.rs`). Sleep past a flush interval instead
/// of polling a specific id/count — the access-boost tests need "whatever
/// bumps are pending have landed," not a per-id deadline poll.
fn settle_counters(_db: &Db) {
    std::thread::sleep(Duration::from_millis(300));
}

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
        graph_boost: false,
        ..RecallQuery::new(scopes.clone(), query, k)
    }
}

// --- labels-filter test support -------------------------------------------

/// Index spec covering both `Memory` and `Entity` labels' `content` prop, so
/// a query can lexically match nodes of either label.
fn spec_with_entity() -> IndexSpec {
    IndexSpec {
        equality: vec![],
        text: vec![
            PropIndex {
                label: "Memory".into(),
                prop: "content".into(),
            },
            PropIndex {
                label: "Entity".into(),
                prop: "content".into(),
            },
        ],
    }
}

fn entity(content: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
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

/// A stable scope shared by every labels-filter test in this file, so
/// `scopes()` (called independently of corpus construction, mirroring the
/// task brief's test bodies) always names the scope the corpus was built
/// under.
fn labels_filter_scope() -> ScopeId {
    static SCOPE: std::sync::OnceLock<ScopeId> = std::sync::OnceLock::new();
    *SCOPE.get_or_init(ScopeId::new)
}

fn scopes() -> ScopeSet {
    ScopeSet::of(&[labels_filter_scope()])
}

/// Builds a fresh db with one `Memory` node and one `Entity` node, both
/// lexically matching `term`, and deliberately UNLINKED (no edge between
/// them) so the graph leg cannot re-introduce one via adjacency and muddy
/// the label-filter precondition.
fn corpus_with_memory_and_entity_matching(term: &str) -> (Db, NodeId, NodeId) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.keep().join("t.redb");
    let db = Db::open_with(db_path, spec_with_entity()).unwrap();
    let s = labels_filter_scope();
    let (memory_id, op_m) = memory(&format!("{term} memory note"), Scope::Id(s));
    let (entity_id, op_e) = entity(&format!("{term} entity record"), Scope::Id(s));
    db.submit(vec![op_m, op_e]).unwrap();
    (db, memory_id, entity_id)
}

/// Two `Memory` nodes with equal textual standing for `term` (same content
/// shape, different id), so any ranking difference between them must come
/// from a post-fusion adjustment (recency/access), not from BM25.
fn corpus_with_two_equal_memories(term: &str) -> (Db, NodeId, NodeId) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.keep().join("t.redb");
    let db = Db::open_with(db_path, spec_with_entity()).unwrap();
    let s = labels_filter_scope();
    let (a_id, op_a) = memory(&format!("{term} memory note"), Scope::Id(s));
    let (b_id, op_b) = memory(&format!("{term} memory note"), Scope::Id(s));
    db.submit(vec![op_a, op_b]).unwrap();
    (db, a_id, b_id)
}

/// One node backdated ~7 days via an explicit `NodeId::from_u128` id (high
/// 48 bits = ULID timestamp, inverting `NodeId::timestamp_ms`'s encoding —
/// see `recency_applies_once_post_fusion`'s `ulid_at` for the same trick),
/// and one freshly-minted node, both matching `term` equally on text.
fn corpus_with_backdated_and_fresh_memory(term: &str) -> (Db, NodeId, NodeId) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.keep().join("t.redb");
    let db = Db::open_with(db_path, spec_with_entity()).unwrap();
    let s = labels_filter_scope();
    const DAY_MS: i64 = 86_400_000;
    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let ulid_at = |ts: i64, n: u128| ((ts as u128) << 80) | n;
    // 7 days, not the deeper backdate `recency_applies_once_post_fusion`
    // uses: at `recency_weight = 0.9` / the default 30-day half-life, a
    // 7-day gap still leaves recency decay shallow enough (~0.87x) for the
    // access boost (bounded below `1 + weight`, i.e. < 2x) to overcome it,
    // while still being deep enough that recency alone picks the fresh node.
    let old_id = NodeId::from_u128(ulid_at(now - 7 * DAY_MS, 1));
    let (fresh_id, op_fresh) = memory(&format!("{term} memory note"), Scope::Id(s));
    let mut props = Props::new();
    props.insert(
        "content".into(),
        PropValue::Str(format!("{term} memory note")),
    );
    let op_old = Op::CreateNode {
        id: old_id,
        scope: Scope::Id(s),
        label: "Memory".into(),
        props,
    };
    db.submit(vec![op_old, op_fresh]).unwrap();
    (db, old_id, fresh_id)
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

#[test]
fn labels_filter_excludes_non_matching_labels() {
    // Corpus: a Memory and an Entity that BOTH match the query tokens,
    // built unlinked so the graph leg can't muddy the precondition.
    let (db, memory_id, entity_id) = corpus_with_memory_and_entity_matching("shared term");

    let unfiltered = db
        .recall(&topodb::RecallQuery {
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    let ids: Vec<_> = unfiltered.iter().map(|(n, _)| n.id).collect();
    assert!(
        ids.contains(&memory_id) && ids.contains(&entity_id),
        "precondition: both fuse in"
    );

    let filtered = db
        .recall(&topodb::RecallQuery {
            labels: Some(vec!["Memory".into()]),
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    assert!(filtered.iter().any(|(n, _)| n.id == memory_id));
    assert!(
        filtered.iter().all(|(n, _)| n.label == "Memory"),
        "no non-Memory label may survive the filter"
    );
}

#[test]
fn labels_filter_all_filtered_is_empty_not_error() {
    let (db, _m, _e) = corpus_with_memory_and_entity_matching("shared term");
    let out = db
        .recall(&topodb::RecallQuery {
            labels: Some(vec!["NoSuchLabel".into()]),
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn zeroed_effective_legs_is_empty_not_error() {
    // Spec's degenerate-but-honest case: validation passes (graph_weight
    // is > 0) but no leg with weight actually runs — text zeroed, no
    // vector supplied, graph_boost off. Must be Ok(empty), not Rejected.
    let (db, _m, _e) = corpus_with_memory_and_entity_matching("shared term");
    let out = db
        .recall(&topodb::RecallQuery {
            text_weight: 0.0,
            graph_boost: false,
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn labels_none_is_unfiltered() {
    let (db, memory_id, entity_id) = corpus_with_memory_and_entity_matching("shared term");
    let out = db
        .recall(&topodb::RecallQuery::new(scopes(), "shared term", 10))
        .unwrap();
    let ids: Vec<_> = out.iter().map(|(n, _)| n.id).collect();
    assert!(ids.contains(&memory_id) && ids.contains(&entity_id));
}

#[test]
fn zero_weight_vector_leg_does_not_ghost_in_vector_only_hits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    // A: lexical match only. B: vector match only (query points at [1,0]).
    let (a, op_a) = memory("login password rotation policy", Scope::Id(s));
    let (b, op_b) = memory("credential storage decision", Scope::Id(s));
    db.submit(vec![op_a, op_b]).unwrap();
    db.submit(vec![
        Op::SetEmbedding {
            id: a,
            model: "m".into(),
            vector: vec![0.0, 1.0],
        },
        Op::SetEmbedding {
            id: b,
            model: "m".into(),
            vector: vec![1.0, 0.0],
        },
    ])
    .unwrap();

    // Precondition: with a live vector leg, the vector-only hit surfaces.
    let mut q = text_only(&scopes, "login", 10);
    q.vector = Some(("m".into(), vec![1.0, 0.0]));
    let hits: Vec<NodeId> = db
        .recall(&q)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert!(
        hits.contains(&b),
        "precondition: vector-only hit must surface with vector_weight > 0"
    );

    // vector_weight == 0.0: the same vector-only node must NOT ghost in at
    // score 0 — it shares no tokens with the query, so only the (now inert)
    // vector leg could have surfaced it.
    let mut q0 = text_only(&scopes, "login", 10);
    q0.vector = Some(("m".into(), vec![1.0, 0.0]));
    q0.vector_weight = 0.0;
    let hits0: Vec<NodeId> = db
        .recall(&q0)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert!(
        !hits0.contains(&b),
        "vector_weight == 0.0 must not admit a vector-only hit: {hits0:?}"
    );
    assert_eq!(hits0, vec![a], "only the live text leg's hit remains");
}

#[test]
fn zero_weight_graph_leg_does_not_ghost_in_neighbor() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (hit, op_h) = memory("deployment pipeline broke on friday", Scope::Id(s));
    // Linked context that shares NO tokens with the query:
    let (linked, op_l) = memory("rollback procedure: revert then redeploy", Scope::Id(s));
    db.submit(vec![op_h, op_l]).unwrap();
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

    // graph_weight == 0.0: the linked neighbor must NOT ghost in even
    // though graph_boost is on and validation passes (graph_weight is a
    // separate field from graph_boost).
    let mut q0 = text_only(&scopes, "deployment friday", 10);
    q0.graph_boost = true;
    q0.graph_weight = 0.0;
    let ids0: Vec<NodeId> = db
        .recall(&q0)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert!(
        !ids0.contains(&linked),
        "graph_weight == 0.0 must not admit the 1-hop neighbor: {ids0:?}"
    );

    // With a live (non-zero) graph_weight, the same neighbor MAY join.
    let mut q1 = text_only(&scopes, "deployment friday", 10);
    q1.graph_boost = true;
    q1.graph_weight = 0.5;
    let ids1: Vec<NodeId> = db
        .recall(&q1)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert!(
        ids1.contains(&linked),
        "graph_weight > 0.0 must let the 1-hop neighbor join: {ids1:?}"
    );
}

#[test]
fn access_weight_zero_is_byte_identical() {
    let (db, _m, _e) = corpus_with_memory_and_entity_matching("shared term");
    let a = db
        .recall(&topodb::RecallQuery::new(scopes(), "shared term", 10))
        .unwrap();
    let b = db
        .recall(&topodb::RecallQuery {
            access_weight: 0.0,
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    let pairs =
        |v: &[(topodb::NodeRecord, f32)]| v.iter().map(|(n, s)| (n.id, *s)).collect::<Vec<_>>();
    assert_eq!(pairs(&a), pairs(&b), "same ids, same scores, same order");
}

#[test]
fn access_boost_lifts_a_frequently_read_node() {
    // Two memories with equal textual standing for the query; bump one's
    // access counter by reading it (db.node() bumps) several times, then
    // recall with access_weight 1.0 and assert the bumped one ranks first.
    let (db, a_id, b_id) = corpus_with_two_equal_memories("shared term");
    for _ in 0..8 {
        let _ = db.node(&scopes(), a_id);
    }
    settle_counters(&db);
    let out = db
        .recall(&topodb::RecallQuery {
            access_weight: 1.0,
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    let first = out.first().map(|(n, _)| n.id);
    assert_eq!(
        first,
        Some(a_id),
        "bumped node must outrank its equal twin (b={b_id:?})"
    );
}

#[test]
fn recency_and_access_factors_multiply() {
    // One OLD node with bumped access vs one FRESH node with none, equal
    // textual standing. With recency_weight high and access_weight 0 the
    // fresh node wins; adding access_weight 1.0 (old node heavily bumped)
    // must lift the old node past it — proving the two factors compose
    // multiplicatively rather than one overwriting the other.
    let (db, old_id, fresh_id) = corpus_with_backdated_and_fresh_memory("shared term");
    for _ in 0..32 {
        let _ = db.node(&scopes(), old_id);
    }
    settle_counters(&db);
    let mut base = topodb::RecallQuery::new(scopes(), "shared term", 10);
    base.options.recency_weight = 0.9;
    // Pin "now" to the fresh node's mint time: the old node's age is then
    // exactly its backdate (~7 days), the fresh node's is ~0.
    base.options.now_ms = Some(fresh_id.timestamp_ms() as i64);
    let recency_only = db.recall(&base).unwrap();
    assert_eq!(recency_only.first().map(|(n, _)| n.id), Some(fresh_id));
    let both = db
        .recall(&topodb::RecallQuery {
            access_weight: 1.0,
            ..base.clone()
        })
        .unwrap();
    assert_eq!(
        both.first().map(|(n, _)| n.id),
        Some(old_id),
        "access boost must be able to overcome recency when counts warrant"
    );
}

#[test]
fn scoring_reads_do_not_bump_counters() {
    let (db, a_id, _b) = corpus_with_two_equal_memories("shared term");
    settle_counters(&db);
    let before = db
        .access_stats(&scopes(), a_id)
        .unwrap()
        .unwrap()
        .access_count;
    // recall with the boost ON reads counters for scoring — which must not bump.
    // NOTE: the LEGS' reads may bump through their own read paths exactly as
    // they do today; to isolate the SCORING read, compare a boosted recall
    // against an unboosted one: the counter delta must be identical.
    let _ = db
        .recall(&topodb::RecallQuery {
            access_weight: 1.0,
            ..topodb::RecallQuery::new(scopes(), "shared term", 10)
        })
        .unwrap();
    settle_counters(&db);
    let after_boosted = db
        .access_stats(&scopes(), a_id)
        .unwrap()
        .unwrap()
        .access_count;
    let _ = db
        .recall(&topodb::RecallQuery::new(scopes(), "shared term", 10))
        .unwrap();
    settle_counters(&db);
    let after_plain = db
        .access_stats(&scopes(), a_id)
        .unwrap()
        .unwrap()
        .access_count;
    assert_eq!(
        after_boosted - before,
        after_plain - after_boosted,
        "the scoring read must add nothing beyond what recall's legs always add"
    );
    let _ = before;
}
