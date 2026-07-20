//! Opening an already-current database must not write to it.
//!
//! `Storage::open_with_options` ran a write transaction unconditionally —
//! opening every table (a no-op when they all exist), seeding the shared
//! scope (idempotent), and committing. On a current-format file that
//! transaction writes nothing, but the commit still fsyncs, and that fsync
//! was measured as the entire remaining cost of a cold open: ~22 ms of a
//! ~43 ms open at 20k nodes, against a ~21 ms raw-redb floor.
//!
//! These tests pin the property behaviourally rather than by timing: a
//! reopen of an unchanged database must leave the file byte-identical.
//! `ensure_index_spec` already established this pattern for its own
//! transaction (the "F9d" read-only precheck); this covers the other one.

use std::path::Path;

use topodb::{Db, NodeId, Op, PropValue, Props, Scope, ScopeId, ScopeSet};

fn digest(path: &Path) -> (u64, Vec<u8>) {
    let bytes = std::fs::read(path).expect("read db file");
    (bytes.len() as u64, bytes)
}

fn seed(path: &Path) {
    let db = Db::open(path).expect("open");
    let scope = Scope::Id(ScopeId::new());
    let mut props = Props::new();
    props.insert("name".into(), PropValue::Str("alpha".into()));
    props.insert("rank".into(), PropValue::Int(7));
    db.submit_at(
        vec![Op::CreateNode {
            id: NodeId::new(),
            scope,
            label: "Memory".into(),
            props,
        }],
        1,
    )
    .expect("submit");
}

#[test]
fn reopening_an_unchanged_database_does_not_modify_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ro.redb");

    seed(&path);
    let before = digest(&path);

    // A plain reopen: current format, all tables present, shared scope
    // already seeded, index spec unchanged. Nothing needs writing.
    {
        let _db = Db::open(&path).expect("reopen");
    }
    let after = digest(&path);

    assert_eq!(
        before.0, after.0,
        "reopen changed the file length: {} -> {}",
        before.0, after.0
    );
    assert!(
        before.1 == after.1,
        "reopen modified the file contents despite nothing needing to be written"
    );
}

#[test]
fn repeated_reopens_are_all_no_ops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ro_repeat.redb");

    seed(&path);
    let baseline = digest(&path);

    // The first reopen is the interesting one, but a second and third must
    // not drift either — a fast path that writes only "sometimes" would show
    // up here.
    for i in 0..3 {
        {
            let _db = Db::open(&path).expect("reopen");
        }
        let now = digest(&path);
        assert!(
            baseline.1 == now.1,
            "reopen #{i} modified the file contents"
        );
    }
}

#[test]
fn a_reopened_database_still_reads_correctly() {
    // The fast path must not skip state the engine needs at runtime: the
    // dictionaries and scope registry are loaded after the transaction, and
    // a reopen that silently lost them would still pass the byte-identity
    // assertions above.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ro_reads.redb");

    let id = NodeId::new();
    let scope = Scope::Id(ScopeId::new());
    {
        let db = Db::open(&path).expect("open");
        let mut props = Props::new();
        props.insert("name".into(), PropValue::Str("beta".into()));
        db.submit_at(
            vec![Op::CreateNode {
                id,
                scope,
                label: "Memory".into(),
                props,
            }],
            1,
        )
        .expect("submit");
    }

    let db = Db::open(&path).expect("reopen");
    let scope_id = match scope {
        Scope::Id(v) => v,
        Scope::Shared => unreachable!("test uses a scoped node"),
    };
    let scopes = ScopeSet::of(&[scope_id]);
    let node = db.node(&scopes, id).expect("node survives reopen");
    assert_eq!(node.label.as_str(), "Memory");
    assert_eq!(
        node.props.get("name"),
        Some(&PropValue::Str("beta".into())),
        "props readable after a reopen that took the read-only path"
    );

    // And the reopened handle must still be writable.
    let id2 = NodeId::new();
    db.submit_at(
        vec![Op::CreateNode {
            id: id2,
            scope,
            label: "Memory".into(),
            props: Props::new(),
        }],
        2,
    )
    .expect("write after read-only open");
    assert!(
        db.node(&scopes, id2).is_some(),
        "write after reopen is visible"
    );
}
