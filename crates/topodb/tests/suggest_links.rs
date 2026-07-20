//! Link prediction: `suggest_links` fuses a structural leg (PPR over the
//! 3-hop neighborhood) with a semantic leg (cosine against the target's own
//! stored embedding), excludes self and current 1-hop neighbors, and
//! returns evidence. Spec: 2026-07-19 PPR & link-prediction design.

use topodb::*;

fn open(dir: &tempfile::TempDir) -> Db {
    Db::open(dir.path().join("t.redb")).unwrap()
}

fn mk_node(id: NodeId, scope: Scope) -> Op {
    Op::CreateNode {
        id,
        scope,
        label: "Memory".into(),
        props: Default::default(),
    }
}

fn mk_edge(scope: Scope, from: NodeId, to: NodeId) -> Op {
    Op::CreateEdge {
        id: EdgeId::new(),
        scope,
        ty: "RELATES_TO".into(),
        from,
        to,
        props: Default::default(),
        valid_from: None,
    }
}

/// a—c, a—d, b—c, b—d (a,b share both neighbors, no a—b edge), d—e.
/// Structure-only suggestion for a must be b (two converging paths),
/// with common-neighbor evidence [c, d].
struct StructFixture {
    _dir: tempfile::TempDir,
    db: Db,
    scope: Scope,
    scopes: ScopeSet,
    a: NodeId,
    b: NodeId,
    c: NodeId,
    d: NodeId,
    e: NodeId,
}

fn structural_fixture() -> StructFixture {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let s = ScopeId::new();
    let scope = Scope::Id(s);
    // Deterministic ascending ids (Ulid::new() is NOT monotonic within a
    // ms): c.id < d.id pins the evidence assertion's [c, d] order.
    let v: Vec<NodeId> = (1u128..=5).map(NodeId::from_u128).collect();
    let (a, b, c, d, e) = (v[0], v[1], v[2], v[3], v[4]);
    db.submit(vec![
        mk_node(a, scope),
        mk_node(b, scope),
        mk_node(c, scope),
        mk_node(d, scope),
        mk_node(e, scope),
        mk_edge(scope, a, c),
        mk_edge(scope, a, d),
        mk_edge(scope, b, c),
        mk_edge(scope, b, d),
        mk_edge(scope, d, e),
    ])
    .unwrap();
    StructFixture {
        _dir: dir,
        db,
        scope,
        scopes: ScopeSet::of(&[s]),
        a,
        b,
        c,
        d,
        e,
    }
}

fn q(scopes: &ScopeSet, node: NodeId, k: usize) -> SuggestLinksQuery {
    SuggestLinksQuery {
        scopes: scopes.clone(),
        node,
        k,
        model: None,
        as_of: None,
    }
}

#[test]
fn shared_neighbors_predict_the_missing_edge_with_evidence() {
    let f = structural_fixture();
    let out = f.db.suggest_links(&q(&f.scopes, f.a, 5)).unwrap();
    assert!(!out.is_empty());
    let top = &out[0];
    assert_eq!(top.node.id, f.b, "b shares two neighbors with a — top pick");
    assert_eq!(
        top.common_neighbors,
        vec![f.c, f.d],
        "evidence, id-ascending"
    );
    assert!(top.structural);
    assert!(!top.semantic, "no embeddings in this fixture");
    // e (2 hops via d, one path) may follow, but never outrank b.
}

#[test]
fn self_and_existing_neighbors_are_never_suggested() {
    let f = structural_fixture();
    let out = f.db.suggest_links(&q(&f.scopes, f.a, 10)).unwrap();
    for s in &out {
        assert!(
            s.node.id != f.a && s.node.id != f.c && s.node.id != f.d,
            "self/1-hop neighbors must be excluded, got {:?}",
            s.node.id
        );
    }
}

#[test]
fn semantic_leg_bridges_disconnected_components() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let s = ScopeId::new();
    let scope = Scope::Id(s);
    let p = NodeId::new();
    let qn = NodeId::new();
    let far = NodeId::new();
    db.submit(vec![
        mk_node(p, scope),
        mk_node(qn, scope),
        mk_node(far, scope),
    ])
    .unwrap();
    // p and qn: near-identical embeddings; far: orthogonal. NO edges at all.
    db.submit(vec![
        Op::SetEmbedding {
            id: p,
            model: "m1".into(),
            vector: vec![1.0, 0.0, 0.0, 0.01],
        },
        Op::SetEmbedding {
            id: qn,
            model: "m1".into(),
            vector: vec![0.99, 0.01, 0.0, 0.0],
        },
        Op::SetEmbedding {
            id: far,
            model: "m1".into(),
            vector: vec![0.0, 0.0, 1.0, 0.0],
        },
    ])
    .unwrap();
    let scopes = ScopeSet::of(&[s]);
    let out = db
        .suggest_links(&SuggestLinksQuery {
            scopes: scopes.clone(),
            node: p,
            k: 2,
            model: Some("m1".into()),
            as_of: None,
        })
        .unwrap();
    assert_eq!(
        out[0].node.id, qn,
        "nearest embedding wins across components"
    );
    assert!(out[0].semantic);
    assert!(!out[0].structural, "no graph path to qn exists");
    assert!(out[0].common_neighbors.is_empty());
}

#[test]
fn unknown_model_or_no_embedding_degrades_to_structure_only() {
    let f = structural_fixture();
    let mut query = q(&f.scopes, f.a, 5);
    query.model = Some("no-such-model".into());
    let out = f.db.suggest_links(&query).unwrap();
    assert_eq!(out[0].node.id, f.b, "structural leg alone still works");
}

#[test]
fn out_of_scope_or_missing_target_is_empty_not_an_error() {
    let f = structural_fixture();
    let other = ScopeSet::of(&[ScopeId::new()]);
    assert!(f.db.suggest_links(&q(&other, f.a, 5)).unwrap().is_empty());
    assert!(f
        .db
        .suggest_links(&q(&f.scopes, NodeId::new(), 5))
        .unwrap()
        .is_empty());
}

#[test]
fn out_of_scope_candidates_are_invisible() {
    let f = structural_fixture();
    // A shared-scope rival wired exactly like b — but the query reads only
    // scope s, and shared is NOT in the scope set, so it must not appear.
    let rival = NodeId::new();
    f.db.submit(vec![
        mk_node(rival, Scope::Shared),
        mk_edge(Scope::Shared, rival, f.c),
        mk_edge(Scope::Shared, rival, f.d),
    ])
    .unwrap();
    let out = f.db.suggest_links(&q(&f.scopes, f.a, 10)).unwrap();
    assert!(out.iter().all(|s| s.node.id != rival));
}

#[test]
fn a_closed_edge_no_longer_blocks_the_suggestion() {
    // Deleted-edge recovery + the spec's "temporally-expired edges do not
    // block" clause in one: create a—b, close it, and b must again be the
    // top suggestion (its shared-neighbor signal is intact).
    let f = structural_fixture();
    let eid = EdgeId::new();
    f.db.submit(vec![Op::CreateEdge {
        id: eid,
        scope: f.scope,
        ty: "RELATES_TO".into(),
        from: f.a,
        to: f.b,
        props: Default::default(),
        valid_from: None,
    }])
    .unwrap();
    // While a—b is live, b is a 1-hop neighbor — never suggested.
    let live = f.db.suggest_links(&q(&f.scopes, f.a, 10)).unwrap();
    assert!(live.iter().all(|s| s.node.id != f.b));
    f.db.submit(vec![Op::CloseEdge {
        id: eid,
        valid_to: None,
    }])
    .unwrap();
    let out = f.db.suggest_links(&q(&f.scopes, f.a, 10)).unwrap();
    assert_eq!(
        out[0].node.id, f.b,
        "closed edge must not block; shared neighbors recover the link"
    );
}

#[test]
fn k_zero_is_rejected() {
    let f = structural_fixture();
    match f.db.suggest_links(&q(&f.scopes, f.a, 0)) {
        Err(TopoError::Rejected(_)) => {}
        other => panic!("k == 0 must be Rejected, got {other:?}"),
    }
}

#[test]
fn repeat_calls_are_deterministic() {
    let f = structural_fixture();
    let query = SuggestLinksQuery {
        as_of: Some(1_000_000),
        ..q(&f.scopes, f.a, 10)
    };
    let ids = |v: &[LinkSuggestion]| v.iter().map(|s| s.node.id).collect::<Vec<_>>();
    let one = f.db.suggest_links(&query).unwrap();
    let two = f.db.suggest_links(&query).unwrap();
    assert_eq!(ids(&one), ids(&two));
}
