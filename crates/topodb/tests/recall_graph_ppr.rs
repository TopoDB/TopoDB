//! The PPR graph leg's improvements over flat 1-hop membership:
//! (1) connectivity ranks — a node linked to several top seeds outranks a
//! node dangling off one seed, regardless of mint order; (2) determinism —
//! PPR gives stable tie-breaking across repeated queries with the same now_ms.

use topodb::*;

struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    scopes: ScopeSet,
    /// Linked to ONE seed; LOWER id than x, so the old flat leg
    /// (seed order, then id) ranked it ahead of x.
    y: NodeId,
    /// Linked to ALL THREE seeds — PPR must rank it above y.
    x: NodeId,
    /// Two hops from seed s1 (via x): must stay OUT of the graph leg (GRAPH_HOPS = 1, eval-tuned).
    z: NodeId,
}

fn build() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let s = ScopeId::new();
    let scope = Scope::Id(s);
    let scopes = ScopeSet::of(&[s]);

    // Deterministic ids (Ulid::new() is NOT monotonic within a ms): the
    // flat-leg failure mode needs y.id < x.id, pinned via from_u128.
    let seeds: Vec<NodeId> = (1u128..=3).map(NodeId::from_u128).collect();
    let y = NodeId::from_u128(10);
    let x = NodeId::from_u128(11);
    let z = NodeId::from_u128(12);

    let mut ops = Vec::new();
    for &sid in &seeds {
        let mut props = Props::new();
        props.insert(
            "content".into(),
            PropValue::Str("alpha beta gamma delta".into()),
        );
        ops.push(Op::CreateNode {
            id: sid,
            scope,
            label: "Memory".into(),
            props,
        });
    }
    for &(id, text) in &[(y, "one link only"), (x, "hub node"), (z, "two hops out")] {
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(text.into()));
        ops.push(Op::CreateNode {
            id,
            scope,
            label: "Memory".into(),
            props,
        });
    }
    // x — every seed; y — seed 0 only; z — x only (2 hops from seeds).
    for &from in &seeds {
        ops.push(Op::CreateEdge {
            id: EdgeId::new(),
            scope,
            ty: "RELATES_TO".into(),
            from,
            to: x,
            props: Default::default(),
            valid_from: None,
        });
    }
    ops.push(Op::CreateEdge {
        id: EdgeId::new(),
        scope,
        ty: "RELATES_TO".into(),
        from: seeds[0],
        to: y,
        props: Default::default(),
        valid_from: None,
    });
    ops.push(Op::CreateEdge {
        id: EdgeId::new(),
        scope,
        ty: "RELATES_TO".into(),
        from: x,
        to: z,
        props: Default::default(),
        valid_from: None,
    });
    db.submit(ops).unwrap();

    Fixture {
        _dir: dir,
        db,
        scopes,
        y,
        x,
        z,
    }
}

/// Position of `id` in the hit list; panics if absent.
fn pos(hits: &[(NodeRecord, f32)], id: NodeId) -> usize {
    hits.iter()
        .position(|(n, _)| n.id == id)
        .unwrap_or_else(|| panic!("{id:?} missing from results"))
}

#[test]
fn multi_seed_neighbor_outranks_single_link_despite_mint_order() {
    let f = build();
    // Query matches only the three seed nodes' content; x/y/z reach the
    // results exclusively through the graph leg.
    let hits =
        f.db.recall(&RecallQuery::new(f.scopes.clone(), "alpha beta", 10))
            .unwrap();
    assert!(
        pos(&hits, f.x) < pos(&hits, f.y),
        "x (3 seed links) must outrank y (1 seed link; lower id)"
    );
}

#[test]
fn two_hop_node_stays_outside_the_graph_leg() {
    // GRAPH_HOPS was tuned 2 → 1 by the golden-set eval (see ppr.rs):
    // 2-hop membership let hub-adjacent nodes crowd correct seeds out of
    // top-3. z — reachable only via x, with no text/vector overlap — must
    // therefore stay OUT of recall results entirely: graph membership is
    // 1-hop, PPR only reorders it.
    let f = build();
    let hits =
        f.db.recall(&RecallQuery::new(f.scopes.clone(), "alpha beta", 10))
            .unwrap();
    assert!(
        hits.iter().all(|(n, _)| n.id != f.z),
        "z (2 hops from any seed) must not enter the 1-hop graph leg"
    );
}

#[test]
fn pinned_now_ms_is_deterministic() {
    let f = build();
    let q = RecallQuery {
        options: SearchOptions {
            now_ms: Some(1_000_000),
            ..SearchOptions::default()
        },
        ..RecallQuery::new(f.scopes.clone(), "alpha beta", 10)
    };
    let a = f.db.recall(&q).unwrap();
    let b = f.db.recall(&q).unwrap();
    let ids = |h: &[(NodeRecord, f32)]| h.iter().map(|(n, _)| n.id).collect::<Vec<_>>();
    assert_eq!(ids(&a), ids(&b));
}
