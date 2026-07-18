//! `Db::edges_from` — the scoped edge-listing/supersession primitive: find
//! the open edges a changed fact should close, without a full traverse.

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
fn edges_from_filters_by_target_type_and_openness() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = ScopeId::new();
    let scope = Scope::Id(s);
    let scopes = ScopeSet::of(&[s]);

    let (a, op_a) = node("Entity", scope);
    let (b, op_b) = node("Entity", scope);
    let (c, op_c) = node("Entity", scope);
    db.submit(vec![op_a, op_b, op_c]).unwrap();

    let (e_ab_works, op1) = edge("works_at", scope, a, b);
    let (e_ac_works, op2) = edge("works_at", scope, a, c);
    let (e_ab_about, op3) = edge("about", scope, a, b);
    db.submit(vec![op1, op2, op3]).unwrap();

    // No filters: all three, oldest first (by edge id).
    let all = db.edges_from(&scopes, a, None, None, false).unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.windows(2).all(|w| w[0].id <= w[1].id));

    // Type filter.
    let works: Vec<EdgeId> = db
        .edges_from(&scopes, a, None, Some("works_at"), false)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(works.len(), 2);
    assert!(works.contains(&e_ab_works) && works.contains(&e_ac_works));

    // Target filter.
    let to_b: Vec<EdgeId> = db
        .edges_from(&scopes, a, Some(b), None, false)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(to_b.len(), 2);
    assert!(to_b.contains(&e_ab_works) && to_b.contains(&e_ab_about));

    // Both filters.
    let narrow = db
        .edges_from(&scopes, a, Some(b), Some("works_at"), false)
        .unwrap();
    assert_eq!(narrow.len(), 1);
    assert_eq!(narrow[0].id, e_ab_works);

    // Close one edge: open_only excludes it, the unfiltered listing keeps it.
    db.submit(vec![Op::CloseEdge {
        id: e_ab_works,
        valid_to: None,
    }])
    .unwrap();
    let open_works = db
        .edges_from(&scopes, a, None, Some("works_at"), true)
        .unwrap();
    assert_eq!(open_works.len(), 1);
    assert_eq!(open_works[0].id, e_ac_works);
    assert_eq!(
        db.edges_from(&scopes, a, None, Some("works_at"), false)
            .unwrap()
            .len(),
        2,
        "closed edges stay listed when open_only is false"
    );

    // An unknown type matches nothing (not everything).
    assert!(db
        .edges_from(&scopes, a, None, Some("never_written"), false)
        .unwrap()
        .is_empty());

    // A never-created `from` node yields empty, not an error.
    assert!(db
        .edges_from(&scopes, NodeId::new(), None, None, false)
        .unwrap()
        .is_empty());
}

#[test]
fn edges_from_gates_on_edge_scope() {
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
        .edges_from(&ScopeSet::of(&[s]), a, None, None, true)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, e);

    // A reader whose scope set doesn't include the edge's scope sees nothing
    // — there is no unscoped read.
    assert!(db
        .edges_from(&ScopeSet::of(&[ScopeId::new()]), a, None, None, true)
        .unwrap()
        .is_empty());
}
