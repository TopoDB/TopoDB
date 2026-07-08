use topodb::*;

struct Fixture { db: Db, s1: ScopeId, a: NodeId, b: NodeId, c: NodeId, _dir: tempfile::TempDir }

/// a -ABOUT-> b(shared) -RELATES_TO-> c, edge b->c closed at t=200.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s1 = ScopeId::new();
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let (e1, e2) = (EdgeId::new(), EdgeId::new());
    db.submit_at(vec![
        Op::CreateNode { id: a, scope: Scope::Id(s1), label: "Memory".into(), props: Default::default() },
        Op::CreateNode { id: b, scope: Scope::Shared, label: "Entity".into(), props: Default::default() },
        Op::CreateNode { id: c, scope: Scope::Shared, label: "Entity".into(), props: Default::default() },
        Op::CreateEdge { id: e1, scope: Scope::Id(s1), ty: "ABOUT".into(), from: a, to: b,
                         props: Default::default(), valid_from: None },
        Op::CreateEdge { id: e2, scope: Scope::Shared, ty: "RELATES_TO".into(), from: b, to: c,
                         props: Default::default(), valid_from: None },
    ], 100).unwrap();
    db.submit_at(vec![Op::CloseEdge { id: e2, valid_to: None }], 200).unwrap();
    Fixture { db, s1, a, b, c, _dir: dir }
}

#[test]
fn two_hop_reaches_shared_entity_now_excludes_closed_edge() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]).with_shared();
    let sub = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.a], max_hops: 2,
        edge_types: None, direction: Direction::Out, as_of: None,
    }).unwrap();
    let ids: Vec<_> = sub.nodes.iter().map(|n| n.id).collect();
    assert!(ids.contains(&f.a) && ids.contains(&f.b));
    assert!(!ids.contains(&f.c), "closed edge must not be traversed at now");
}

#[test]
fn as_of_150_sees_the_since_closed_edge() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]).with_shared();
    let sub = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.a], max_hops: 2,
        edge_types: None, direction: Direction::Out, as_of: Some(150),
    }).unwrap();
    assert!(sub.nodes.iter().any(|n| n.id == f.c), "as_of read must see historical edge");
}

#[test]
fn scope_excluded_traversal_stops_at_boundary() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]); // NO shared
    let sub = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.a], max_hops: 2,
        edge_types: None, direction: Direction::Out, as_of: Some(150),
    }).unwrap();
    assert_eq!(sub.nodes.len(), 1, "b is Shared and shared not in scope set");
}

#[test]
fn hop_cap_enforced() {
    let f = fixture();
    let q = TraversalQuery { scopes: ScopeSet::of(&[f.s1]), seeds: vec![f.a], max_hops: 5,
                             edge_types: None, direction: Direction::Out, as_of: None };
    assert!(matches!(f.db.traverse(&q), Err(TopoError::Rejected(_))));
}

#[test]
fn max_hops_zero_is_rejected() {
    let f = fixture();
    let q = TraversalQuery { scopes: ScopeSet::of(&[f.s1]), seeds: vec![f.a], max_hops: 0,
                             edge_types: None, direction: Direction::Out, as_of: None };
    assert!(matches!(f.db.traverse(&q), Err(TopoError::Rejected(_))));
}

/// Walk the fixture *backwards* from `c`: `In` must follow `inn`, reaching
/// b (via e2) and then a (via e1). Read `as_of` 150 so the since-closed e2 is
/// still open.
#[test]
fn direction_in_traverses_backwards() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]).with_shared();
    let sub = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.c], max_hops: 2,
        edge_types: None, direction: Direction::In, as_of: Some(150),
    }).unwrap();
    let ids: Vec<_> = sub.nodes.iter().map(|n| n.id).collect();
    assert!(ids.contains(&f.c) && ids.contains(&f.b) && ids.contains(&f.a),
            "In-traversal from c must reach b and a, got {ids:?}");
}

/// `Both` from b reaches a (via `inn`/e1) and c (via `out`/e2). e2 is
/// encountered from both b->c and c->b; the `visited`/`result_edges` sets must
/// dedup it so every node and edge appears exactly once in the Subgraph.
#[test]
fn direction_both_dedups_nodes_and_edges() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]).with_shared();
    let sub = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.b], max_hops: 2,
        edge_types: None, direction: Direction::Both, as_of: Some(150),
    }).unwrap();
    assert_eq!(sub.nodes.len(), 3, "a, b, c each exactly once");
    assert_eq!(sub.edges.len(), 2, "e1, e2 each exactly once (e2 not double-counted)");
    let mut node_ids: Vec<_> = sub.nodes.iter().map(|n| n.id).collect();
    node_ids.sort(); node_ids.dedup();
    assert_eq!(node_ids.len(), 3, "node set must be duplicate-free");
    let mut edge_ids: Vec<_> = sub.edges.iter().map(|e| e.id).collect();
    edge_ids.sort(); edge_ids.dedup();
    assert_eq!(edge_ids.len(), 2, "edge set must be duplicate-free");
}

/// `edge_types: Some(["ABOUT"])` must exclude the RELATES_TO edge b->c, so the
/// traversal stops at b and never reaches c.
#[test]
fn edge_types_filter_excludes_other_types() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]).with_shared();
    let sub = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.a], max_hops: 3,
        edge_types: Some(vec!["ABOUT".into()]), direction: Direction::Out, as_of: Some(150),
    }).unwrap();
    let ids: Vec<_> = sub.nodes.iter().map(|n| n.id).collect();
    assert!(ids.contains(&f.b), "ABOUT edge a->b must be traversed");
    assert!(!ids.contains(&f.c), "RELATES_TO edge b->c must be excluded by edge_types filter");
    assert!(sub.edges.iter().all(|e| e.ty == "ABOUT"), "only ABOUT edges may appear");
}

/// Temporal window boundaries are half-open `[valid_from, valid_to)`: e2 has
/// valid_from=100, valid_to=200. At t == valid_from the edge IS traversable
/// (inclusive lower bound); at t == valid_to it is NOT (exclusive upper bound).
#[test]
fn temporal_boundaries_inclusive_from_exclusive_to() {
    let f = fixture();
    let scopes = ScopeSet::of(&[f.s1]).with_shared();
    let at_from = f.db.traverse(&TraversalQuery {
        scopes: scopes.clone(), seeds: vec![f.b], max_hops: 1,
        edge_types: None, direction: Direction::Out, as_of: Some(100),
    }).unwrap();
    assert!(at_from.nodes.iter().any(|n| n.id == f.c),
            "at t == valid_from (100) the edge must be traversable");
    let at_to = f.db.traverse(&TraversalQuery {
        scopes, seeds: vec![f.b], max_hops: 1,
        edge_types: None, direction: Direction::Out, as_of: Some(200),
    }).unwrap();
    assert!(!at_to.nodes.iter().any(|n| n.id == f.c),
            "at t == valid_to (200) the edge must not be traversable");
}

#[test]
fn snapshot_is_not_part_of_the_public_read_api() {
    // Compile-time contract: the supported public read APIs are the scoped
    // ones. This test exercises the debug seam that replaces `snapshot()`
    // for tests, and asserts it returns a consistent view.
    let f = fixture();
    let snap = f.db.debug_snapshot();
    assert!(snap.debug_nodes().contains_key(&f.a));
    assert!(snap.debug_edges().len() >= 2);
}
