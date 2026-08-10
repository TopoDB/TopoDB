use topodb::{
    Db, Direction, EdgeRecord, NodeId, Op, Props, Scope, ScopeId, ScopeSet, TimeAxis,
    TraversalQuery,
};
use topodb_sgh::store::supersede::{link_superseding, link_superseding_with};
use topodb_sgh::store::SghError;

fn node(db: &Db, scope: Scope, label: &str, t: i64) -> NodeId {
    let id = NodeId::new();
    db.submit_at(
        vec![Op::CreateNode {
            id,
            scope,
            label: label.into(),
            props: Props::new(),
        }],
        t,
    )
    .expect("create node");
    id
}

/// Edges of type `ty` out of `from` that are open as of `as_of` — i.e. the
/// engine's own answer to "what does `from` currently point at via `ty`".
/// There is no `edges_from` method on `Db`; `traverse` (1 hop, `Direction::Out`,
/// a type filter, and an explicit `as_of`) is the public read primitive that
/// gives us this.
fn open_edges(db: &Db, scopes: &ScopeSet, from: NodeId, ty: &str, as_of: i64) -> Vec<EdgeRecord> {
    let sg = db
        .traverse(&TraversalQuery {
            scopes: scopes.clone(),
            seeds: vec![from],
            max_hops: 1,
            edge_types: Some(vec![ty.into()]),
            direction: Direction::Out,
            as_of: Some(as_of),
            time_axis: TimeAxis::Valid,
        })
        .unwrap();
    sg.edges.into_iter().filter(|e| e.from == from).collect()
}

/// Every edge of type `ty` out of `from`, open or closed — full history, via
/// the `#[doc(hidden)]` debug dump (there is no `as_of`-unfiltered public
/// read; a single `as_of` window structurally can't return both a closed
/// edge's tenure and an open edge's tenure at once).
fn all_edges(db: &Db, from: NodeId, ty: &str) -> Vec<EdgeRecord> {
    db.debug_dump_edges()
        .into_iter()
        .filter(|e| e.from == from && e.ty.as_str() == ty)
        .collect()
}

#[test]
fn superseding_closes_the_previous_edge_of_the_same_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let sid = ScopeId::new();
    let scope = Scope::Id(sid);
    let scopes = ScopeSet::of(&[sid]);

    let step = node(&db, scope, "SghNode", 1);
    let running = node(&db, scope, "SghState", 2);
    let done = node(&db, scope, "SghState", 3);

    link_superseding(&db, scope, step, running, "HAS_STATE", 10).unwrap();
    link_superseding(&db, scope, step, done, "HAS_STATE", 20).unwrap();

    let open = open_edges(&db, &scopes, step, "HAS_STATE", 20);
    assert_eq!(open.len(), 1, "exactly one open state edge");
    assert_eq!(open[0].to, done);

    let all = all_edges(&db, step, "HAS_STATE");
    assert_eq!(all.len(), 2, "history is preserved, not overwritten");
    let closed = all.iter().find(|e| e.to == running).unwrap();
    assert_eq!(
        closed.valid_to,
        Some(20),
        "old edge closed at the new edge's timestamp"
    );
}

#[test]
fn relinking_the_same_target_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let sid = ScopeId::new();
    let scope = Scope::Id(sid);

    let step = node(&db, scope, "SghNode", 1);
    let running = node(&db, scope, "SghState", 2);

    let a = link_superseding(&db, scope, step, running, "HAS_STATE", 10).unwrap();
    let b = link_superseding(&db, scope, step, running, "HAS_STATE", 20).unwrap();

    assert_eq!(a, b, "no duplicate edge for an unchanged fact");
    let all = all_edges(&db, step, "HAS_STATE");
    assert_eq!(all.len(), 1);
}

#[test]
fn other_edge_types_are_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let sid = ScopeId::new();
    let scope = Scope::Id(sid);
    let scopes = ScopeSet::of(&[sid]);

    let step = node(&db, scope, "SghNode", 1);
    let dep = node(&db, scope, "SghNode", 2);
    let running = node(&db, scope, "SghState", 3);
    let done = node(&db, scope, "SghState", 4);

    link_superseding(&db, scope, step, dep, "DEPENDS_ON", 10).unwrap();
    link_superseding(&db, scope, step, running, "HAS_STATE", 11).unwrap();
    link_superseding(&db, scope, step, done, "HAS_STATE", 12).unwrap();

    let deps = open_edges(&db, &scopes, step, "DEPENDS_ON", 12);
    assert_eq!(deps.len(), 1, "DEPENDS_ON survives HAS_STATE supersession");
}

/// A nonexistent endpoint must fail immediately with `MissingEndpoint`, not
/// burn all `MAX_ATTEMPTS` retries and report a misleading `Contended`. Before
/// the endpoint pre-check, `Db::submit_at` would reject the `CreateEdge` (its
/// `to` doesn't exist) with the same `TopoError::Rejected` used for a lost
/// race, and the retry loop had no way to tell the two apart — so this would
/// have looped 16 times and returned `SghError::Contended { attempts: 16 }`.
#[test]
fn nonexistent_target_fails_fast_with_missing_endpoint_not_contended() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let sid = ScopeId::new();
    let scope = Scope::Id(sid);

    let step = node(&db, scope, "SghNode", 1);
    let ghost = NodeId::new(); // never created

    let err = link_superseding(&db, scope, step, ghost, "HAS_STATE", 10).unwrap_err();
    match err {
        SghError::MissingEndpoint { node } => assert_eq!(node, ghost),
        other => panic!("expected MissingEndpoint {{ node: ghost }}, got {other:?}"),
    }
}

/// Same fast-fail behavior when the *source* is the missing endpoint.
#[test]
fn nonexistent_source_fails_fast_with_missing_endpoint_not_contended() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let sid = ScopeId::new();
    let scope = Scope::Id(sid);

    let ghost = NodeId::new(); // never created
    let running = node(&db, scope, "SghState", 1);

    let err = link_superseding(&db, scope, ghost, running, "HAS_STATE", 10).unwrap_err();
    match err {
        SghError::MissingEndpoint { node } => assert_eq!(node, ghost),
        other => panic!("expected MissingEndpoint {{ node: ghost }}, got {other:?}"),
    }
}

/// The composed variant lands prelude node + superseding edge in ONE batch:
/// after the call, the node exists AND the old edge is closed AND the new
/// edge is open — and the op log shows them at the same timestamp.
#[test]
fn link_superseding_with_prelude_is_single_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let sid = ScopeId::new();
    let scope = Scope::Id(sid);
    let scopes = ScopeSet::of(&[sid]);

    let from = node(&db, scope, "SghNode", 1);
    let to1 = node(&db, scope, "SghState", 2);

    link_superseding(&db, scope, from, to1, "T", 10).unwrap();

    // to2 is minted here — NOT created yet — and only comes into existence
    // via the prelude op inside link_superseding_with's single batch.
    let to2 = NodeId::new();
    link_superseding_with(
        &db,
        scope,
        from,
        to2,
        "T",
        50,
        vec![Op::CreateNode {
            id: to2,
            scope,
            label: "L".into(),
            props: Props::new(),
        }],
    )
    .unwrap();

    assert!(db.node(&scopes, to2).is_some(), "prelude CreateNode landed");

    let open = open_edges(&db, &scopes, from, "T", 50);
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].to, to2);

    let all = all_edges(&db, from, "T");
    assert_eq!(all.len(), 2, "history preserved");
    let closed = all.iter().find(|e| e.to == to1).unwrap();
    assert_eq!(
        closed.valid_to,
        Some(50),
        "old edge closed at the same timestamp the new one opens — same batch"
    );
    let opened = all.iter().find(|e| e.to == to2).unwrap();
    assert_eq!(opened.valid_from, 50);
}
