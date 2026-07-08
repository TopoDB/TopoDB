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
