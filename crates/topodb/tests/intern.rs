//! Public-behavior regression net for the intern-journal revert (F9-11 Task
//! 4): a batch that interns a NEW dictionary entry (here, a node label) and
//! then fails partway through must leave NO trace of that intern behind —
//! neither in the on-disk DICT table nor in the in-memory `Dicts` mirror
//! `apply_batch` consults on every subsequent batch. This is the regression
//! net for Tasks 4-6 (journal+revert, guards-before-commit, group commit):
//! it must keep passing across all three.
use topodb::{Db, NodeId, Op, Scope, ScopeId, ScopeSet};

#[test]
fn aborted_batch_leaves_no_phantom_interns() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let scope_id = ScopeId::new();
    let scope = Scope::Id(scope_id);

    // A batch that interns a brand-new label ("PhantomLabel" has never been
    // seen before) via CreateNode, then fails on its second op: RemoveNode
    // of a NodeId that was never created. The whole batch must reject —
    // nothing commits, including the label intern.
    let n = NodeId::new();
    let res = db.submit(vec![
        Op::CreateNode {
            id: n,
            scope,
            label: "PhantomLabel".into(),
            props: Default::default(),
        },
        Op::RemoveNode { id: NodeId::new() }, // fails: absent
    ]);
    assert!(res.is_err(), "batch must reject");

    // The phantom label must be fully gone from both the on-disk DICT table
    // and the in-memory mirror: a fresh, independent, successful use of the
    // SAME label string must intern it again from scratch (not silently
    // reuse a leftover in-memory id that the disk row never saw), and a read
    // must see EXACTLY the one node created by this second batch — not two,
    // which would mean the aborted CreateNode's label pointed at a phantom
    // id that somehow made a node visible.
    let created = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id: created,
        scope,
        label: "PhantomLabel".into(),
        props: Default::default(),
    }])
    .unwrap();

    let scopes = ScopeSet::of(&[scope_id]);
    let found = db.nodes_by_label(&scopes, "PhantomLabel");
    assert_eq!(
        found.len(),
        1,
        "expected exactly one PhantomLabel node, got {found:?}"
    );
    assert_eq!(found[0].id, created);
}
