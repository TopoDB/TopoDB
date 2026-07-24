//! `Db::edges_to` — incoming edge listing, mirroring `edges_from` for reverse
//! adjacency: find the open edges pointing TO a changed fact.

use topodb::*;

fn node(label: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    (
        id,
        Op::CreateNode {
            id,
            scope,
            label: label.into(),
            props: Props::new(),
        },
    )
}

fn edge(ty: &str, scope: Scope, from: NodeId, to: NodeId) -> (EdgeId, Op) {
    let id = EdgeId::new();
    (
        id,
        Op::CreateEdge {
            id,
            scope,
            ty: ty.into(),
            from,
            to,
            props: Props::new(),
            valid_from: None,
        },
    )
}

#[test]
fn edges_to_filters_by_source_type_and_openness() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scope = Scope::Id(s);
    let scopes = ScopeSet::of(&[s]);

    let (a, op_a) = node("Entity", scope);
    let (b, op_b) = node("Entity", scope);
    let (c, op_c) = node("Entity", scope);
    let (d, op_d) = node("Entity", scope);
    db.submit(vec![op_a, op_b, op_c, op_d]).unwrap();

    // A-e1->B, C-e2->B (different types), B-e3->D (not pointing to B, so should not appear)
    let (e1, op1) = edge("works_at", scope, a, b);
    let (e2, op2) = edge("about", scope, c, b);
    let (e3, op3) = edge("knows", scope, b, d);
    db.submit(vec![op1, op2, op3]).unwrap();

    // No filters: return e1 and e2 (pointing TO b), not e3, oldest first (by edge id).
    let all = db.edges_to(&scopes, b, None, None, false).unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.windows(2).all(|w| w[0].id <= w[1].id));
    let all_ids: Vec<EdgeId> = all.iter().map(|e| e.id).collect();
    assert!(all_ids.contains(&e1) && all_ids.contains(&e2));
    assert!(!all_ids.contains(&e3));

    // Type filter: only "works_at" edges pointing to b.
    let works: Vec<EdgeId> = db
        .edges_to(&scopes, b, None, Some("works_at"), false)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(works.len(), 1);
    assert_eq!(works[0], e1);

    // Source filter: only edges from A pointing to B.
    let from_a: Vec<EdgeId> = db
        .edges_to(&scopes, b, Some(a), None, false)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_a[0], e1);

    // Both filters: only works_at edges from A to B.
    let narrow = db
        .edges_to(&scopes, b, Some(a), Some("works_at"), false)
        .unwrap();
    assert_eq!(narrow.len(), 1);
    assert_eq!(narrow[0].id, e1);

    // Close one edge: open_only excludes it, the unfiltered listing keeps it.
    db.submit(vec![Op::CloseEdge {
        id: e1,
        valid_to: None,
    }])
    .unwrap();
    let open_to_b = db.edges_to(&scopes, b, None, None, true).unwrap();
    assert_eq!(open_to_b.len(), 1);
    assert_eq!(open_to_b[0].id, e2);
    assert_eq!(
        db.edges_to(&scopes, b, None, None, false).unwrap().len(),
        2,
        "closed edges stay listed when open_only is false"
    );

    // An unknown type matches nothing (not everything).
    assert!(db
        .edges_to(&scopes, b, None, Some("never_written"), false)
        .unwrap()
        .is_empty());

    // A never-created `to` node yields empty, not an error.
    assert!(db
        .edges_to(&scopes, NodeId::new(), None, None, false)
        .unwrap()
        .is_empty());
}

#[test]
fn edges_to_gates_on_edge_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scope = Scope::Id(s);

    let (a, op_a) = node("Entity", scope);
    let (b, op_b) = node("Entity", scope);
    db.submit(vec![op_a, op_b]).unwrap();
    let (e, op_e) = edge("knows", scope, a, b);
    db.submit(vec![op_e]).unwrap();

    // In scope: visible.
    let hits = db
        .edges_to(&ScopeSet::of(&[s]), b, None, None, true)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, e);

    // A reader whose scope set doesn't include the edge's scope sees nothing
    // — there is no unscoped read.
    assert!(db
        .edges_to(&ScopeSet::of(&[ScopeId::new()]), b, None, None, true)
        .unwrap()
        .is_empty());
}
