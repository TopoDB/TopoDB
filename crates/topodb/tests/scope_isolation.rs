//! Read isolation: the core promise of a per-project graph with a shared
//! layer. A read scoped to project A must never see project B's data; a
//! read must see the shared layer only when it opts in via `with_shared()`;
//! and a read over multiple scopes sees exactly their union. Covers the node
//! read paths (`node`, `nodes_by_label`) and full-text search, since a leak
//! in any one of them is a cross-project data disclosure.

use topodb::*;

struct Fx {
    db: Db,
    _dir: tempfile::TempDir,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    // FTS index on Note.content so search_text has something to match.
    let spec = IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Note".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    Fx { db, _dir: dir }
}

fn note(id: NodeId, scope: Scope, content: &str) -> Op {
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    Op::CreateNode {
        id,
        scope,
        label: "Note".into(),
        props,
    }
}

#[test]
fn a_project_read_never_sees_another_projects_nodes() {
    let f = fx();
    let (a, b) = (ScopeId::new(), ScopeId::new());
    let (na, nb) = (NodeId::new(), NodeId::new());
    f.db.submit(vec![
        note(na, Scope::Id(a), "alpha note"),
        note(nb, Scope::Id(b), "bravo note"),
    ])
    .unwrap();

    // nodes_by_label is scoped.
    let in_a: Vec<_> =
        f.db.nodes_by_label(&ScopeSet::of(&[a]), "Note")
            .into_iter()
            .map(|r| r.id)
            .collect();
    assert_eq!(in_a, vec![na], "scope A must see only A's node");
    let in_b: Vec<_> =
        f.db.nodes_by_label(&ScopeSet::of(&[b]), "Note")
            .into_iter()
            .map(|r| r.id)
            .collect();
    assert_eq!(in_b, vec![nb], "scope B must see only B's node");

    // Direct id fetch is scoped too: A's set cannot resolve B's node.
    assert!(
        f.db.node(&ScopeSet::of(&[a]), nb).is_none(),
        "fetching B's node id under scope A must return None (no cross-scope id leak)"
    );
    assert!(
        f.db.node(&ScopeSet::of(&[a]), na).is_some(),
        "A's own node resolves under scope A"
    );
}

#[test]
fn full_text_search_is_scoped() {
    let f = fx();
    let (a, b) = (ScopeId::new(), ScopeId::new());
    f.db.submit(vec![
        note(NodeId::new(), Scope::Id(a), "shared secret alpha"),
        note(NodeId::new(), Scope::Id(b), "shared secret bravo"),
    ])
    .unwrap();

    let hits_a = f.db.search_text(&ScopeSet::of(&[a]), "secret", 10).unwrap();
    assert_eq!(hits_a.len(), 1, "search in A matches only A's note");
    let hits_b = f.db.search_text(&ScopeSet::of(&[b]), "secret", 10).unwrap();
    assert_eq!(hits_b.len(), 1, "search in B matches only B's note");
    // The union sees both.
    let hits_ab =
        f.db.search_text(&ScopeSet::of(&[a, b]), "secret", 10)
            .unwrap();
    assert_eq!(hits_ab.len(), 2, "search over {{A,B}} matches both");
}

#[test]
fn the_shared_layer_is_visible_only_when_opted_in() {
    let f = fx();
    let a = ScopeId::new();
    let (shared_node, proj_node) = (NodeId::new(), NodeId::new());
    f.db.submit(vec![
        note(shared_node, Scope::Shared, "shared knowledge"),
        note(proj_node, Scope::Id(a), "project knowledge"),
    ])
    .unwrap();

    // Project scope WITHOUT shared: only the project's own node.
    let only_proj: Vec<_> =
        f.db.nodes_by_label(&ScopeSet::of(&[a]), "Note")
            .into_iter()
            .map(|r| r.id)
            .collect();
    assert_eq!(
        only_proj,
        vec![proj_node],
        "without with_shared(), the shared node must NOT appear"
    );

    // Project scope WITH shared: both, and the shared node is reachable.
    let with_shared: std::collections::HashSet<_> =
        f.db.nodes_by_label(&ScopeSet::of(&[a]).with_shared(), "Note")
            .into_iter()
            .map(|r| r.id)
            .collect();
    assert!(
        with_shared.contains(&shared_node) && with_shared.contains(&proj_node),
        "with_shared() must expose the shared node alongside the project's own"
    );
}

#[test]
fn a_shared_node_is_visible_from_any_project_that_opts_in() {
    let f = fx();
    let (a, b) = (ScopeId::new(), ScopeId::new());
    let shared_node = NodeId::new();
    f.db.submit(vec![note(shared_node, Scope::Shared, "common")])
        .unwrap();

    for (name, s) in [("A", a), ("B", b)] {
        let seen = f.db.node(&ScopeSet::of(&[s]).with_shared(), shared_node);
        assert!(
            seen.is_some(),
            "the shared node must be visible from project {name} with_shared()"
        );
        // ...but not without opting into shared.
        assert!(
            f.db.node(&ScopeSet::of(&[s]), shared_node).is_none(),
            "the shared node must be hidden from project {name} without with_shared()"
        );
    }
}
