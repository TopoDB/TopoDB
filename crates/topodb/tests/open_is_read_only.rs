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

/// The read-only fast path must never swallow a migration. A v5 file has a
/// stale `format_version` and no `LABEL_INDEX` table, so `open_needs_write`
/// must report true and let `migrate_v5_to_v6` run; only the *second* open,
/// once the file is genuinely current, may take the fast path.
///
/// This is the specific hazard the fast path introduces, and the existing
/// `format_fixture.rs` coverage cannot see it: that test opens the fixture
/// once and checks reads, which would still pass if a migration were skipped
/// but the data happened to remain readable.
#[test]
fn v5_fixture_migrates_on_first_open_then_takes_the_fast_path() {
    use topodb::{IndexSpec, PropIndex, ScopeSet};

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v5.redb");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v5.redb");
    std::fs::copy(&src, &path).expect("copy fixture"); // never open the committed file read-write

    // The spec the fixture was written with, so `ensure_index_spec` has no
    // reindex of its own to do and the only reason to write is the migration.
    let spec = IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    };

    let before_migration = digest(&path);
    {
        let db = Db::open_with(&path, spec.clone()).expect("open v5 fixture");
        let scopes = ScopeSet::of(&[topodb::ScopeId::from_u128(1)]);

        // v6's whole point: the label index must exist and be populated. On a
        // v5 file that can only be true if the migration actually ran.
        let entities = db.nodes_by_label(&scopes, "Entity");
        assert_eq!(
            entities.len(),
            1,
            "LABEL_INDEX must be populated by the v5 -> v6 migration"
        );
        assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    }
    let after_migration = digest(&path);

    assert!(
        before_migration.1 != after_migration.1,
        "a v5 file must be migrated on open, not skipped by the read-only fast path"
    );

    // Now current: the second open has nothing to do and must not write.
    {
        let _db = Db::open_with(&path, spec.clone()).expect("reopen migrated file");
    }
    let after_reopen = digest(&path);
    assert!(
        after_migration.1 == after_reopen.1,
        "reopening the migrated file must take the read-only fast path"
    );

    // And the migrated data must still be intact and queryable after that
    // fast-path open.
    let db = Db::open_with(&path, spec).expect("final open");
    let scopes = ScopeSet::of(&[topodb::ScopeId::from_u128(1)]);
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(
        db.search_text(&scopes, "databases", 10)
            .expect("text search")
            .len(),
        1,
        "text index survives migration + fast-path reopen"
    );
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
