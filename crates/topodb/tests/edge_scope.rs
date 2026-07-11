//! D3: an edge's scope is validated against its endpoints' at submit time.

use topodb::*;

struct Fx {
    db: Db,
    _dir: tempfile::TempDir,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    Fx { db, _dir: dir }
}

fn node(id: NodeId, scope: Scope) -> Op {
    Op::CreateNode {
        id,
        scope,
        label: "Entity".into(),
        props: Default::default(),
    }
}

fn edge(id: EdgeId, scope: Scope, from: NodeId, to: NodeId) -> Op {
    Op::CreateEdge {
        id,
        scope,
        ty: "KNOWS".into(),
        from,
        to,
        props: Default::default(),
        valid_from: None,
    }
}

#[test]
fn unrelated_edge_scope_between_project_nodes_is_rejected() {
    let f = fx();
    let (a, p) = (ScopeId::new(), ScopeId::new());
    let (x, y) = (NodeId::new(), NodeId::new());

    let err =
        f.db.submit(vec![
            node(x, Scope::Id(a)),
            node(y, Scope::Id(a)),
            edge(EdgeId::new(), Scope::Id(p), x, y),
        ])
        .unwrap_err();

    assert!(
        matches!(err, TopoError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );
    let sg =
        f.db.traverse(&TraversalQuery {
            scopes: ScopeSet::of(&[a]),
            seeds: vec![x],
            max_hops: 1,
            edge_types: None,
            direction: Direction::Both,
            as_of: None,
        })
        .unwrap();
    assert!(
        sg.edges.is_empty() && sg.nodes.is_empty(),
        "a rejected batch must leave storage untouched"
    );
}

#[test]
fn unrelated_edge_scope_between_project_and_shared_node_is_rejected() {
    let f = fx();
    let (a, p) = (ScopeId::new(), ScopeId::new());
    let (x, y) = (NodeId::new(), NodeId::new());

    let err =
        f.db.submit(vec![
            node(x, Scope::Id(a)),
            node(y, Scope::Shared),
            edge(EdgeId::new(), Scope::Id(p), x, y),
        ])
        .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)), "got {err:?}");
}

#[test]
fn unrelated_edge_scope_is_rejected_against_preexisting_nodes() {
    let f = fx();
    let (a, p) = (ScopeId::new(), ScopeId::new());
    let (x, y) = (NodeId::new(), NodeId::new());

    f.db.submit(vec![node(x, Scope::Id(a)), node(y, Scope::Id(a))])
        .expect("nodes alone are fine");

    let err =
        f.db.submit(vec![edge(EdgeId::new(), Scope::Id(p), x, y)])
            .unwrap_err();
    assert!(matches!(err, TopoError::Rejected(_)), "got {err:?}");
}

#[test]
fn the_rejection_message_names_the_offending_scope_and_the_fix() {
    let f = fx();
    let (a, p) = (ScopeId::new(), ScopeId::new());
    let (x, y) = (NodeId::new(), NodeId::new());

    let err =
        f.db.submit(vec![
            node(x, Scope::Id(a)),
            node(y, Scope::Id(a)),
            edge(EdgeId::new(), Scope::Id(p), x, y),
        ])
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains(&p.to_string()),
        "must name the bad scope: {msg}"
    );
    assert!(
        msg.contains(&a.to_string()),
        "must name the endpoint scope: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("shared"),
        "must state the fix (scope it A or Shared): {msg}"
    );
}

#[test]
fn project_scoped_edge_between_shared_nodes_commits_and_is_project_private() {
    let f = fx();
    let (p, other) = (ScopeId::new(), ScopeId::new());
    let (x, y) = (NodeId::new(), NodeId::new());

    f.db.submit(vec![
        node(x, Scope::Shared),
        node(y, Scope::Shared),
        edge(EdgeId::new(), Scope::Id(p), x, y),
    ])
    .expect("a project-scoped edge over two shared nodes is legal");

    let seen = |scopes: ScopeSet| {
        f.db.traverse(&TraversalQuery {
            scopes,
            seeds: vec![x],
            max_hops: 1,
            edge_types: None,
            direction: Direction::Both,
            as_of: None,
        })
        .unwrap()
        .edges
        .len()
    };

    assert_eq!(
        seen(ScopeSet::of(&[p]).with_shared()),
        1,
        "the writing project must see its own edge"
    );
    assert_eq!(
        seen(ScopeSet::of(&[other]).with_shared()),
        0,
        "another project must not see it"
    );
}

#[test]
fn shared_edge_between_project_nodes_commits() {
    let f = fx();
    let a = ScopeId::new();
    let (x, y) = (NodeId::new(), NodeId::new());
    f.db.submit(vec![
        node(x, Scope::Id(a)),
        node(y, Scope::Id(a)),
        edge(EdgeId::new(), Scope::Shared, x, y),
    ])
    .expect("a Shared edge between two project nodes is legal");
}

#[test]
fn edge_scoped_to_its_project_endpoint_commits() {
    let f = fx();
    let a = ScopeId::new();
    let (x, y) = (NodeId::new(), NodeId::new());
    f.db.submit(vec![
        node(x, Scope::Id(a)),
        node(y, Scope::Shared),
        edge(EdgeId::new(), Scope::Id(a), x, y),
    ])
    .expect("an edge scoped to its project endpoint is legal");
}
