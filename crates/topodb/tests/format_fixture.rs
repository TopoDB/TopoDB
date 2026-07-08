//! Committed v1 format fixture: a small `.redb` file checked into
//! `tests/fixtures/v1.redb` that pins the on-disk layout described in
//! `FORMAT.md`. Two tests:
//!
//! - `regenerate_fixture` (`#[ignore]`): (re)writes the fixture. Run it
//!   explicitly with `cargo test -p topodb --test format_fixture --
//!   --ignored regenerate` whenever the v1 layout intentionally changes (and
//!   bump `FORMAT_VERSION` in `storage.rs` + `FORMAT.md` alongside it — this
//!   test does not do that for you).
//! - `v1_fixture_opens_and_reads` (runs in the normal suite): copies the
//!   committed fixture to a tempdir and asserts the documented queries
//!   (`nodes_by_prop`, `search_text`, `search_vector`, `current_seq`) still
//!   see the expected content. The committed file is NEVER opened
//!   read-write in place — `open_with` can write to META (format_version/
//!   index_spec stamping), which would dirty the committed bytes on every
//!   test run.
//!
//! Node/scope ids use `NodeId::from_u128`/`ScopeId::from_u128`
//! (`#[doc(hidden)]` debug-seam constructors added in `ids.rs` for exactly
//! this purpose) rather than `Ulid::new()`, so the fixture's *content* is
//! reproducible across regenerations. The raw `.redb` bytes are NOT
//! guaranteed byte-for-byte stable (redb's on-disk layout has padding/free
//! space that isn't part of the content contract) — only the query results
//! below are.

use topodb::*;

/// Regenerate with: cargo test -p topodb --test format_fixture -- --ignored regenerate
#[test]
#[ignore]
fn regenerate_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1.redb");
    let _ = std::fs::remove_file(&path);
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
    let db = Db::open_with(&path, spec).unwrap();
    // Fixed ids so the fixture's CONTENT is reproducible across regenerations
    // (the raw bytes are not — see the module doc comment).
    let s = ScopeId::from_u128(1);
    let n1 = NodeId::from_u128(10);
    let n2 = NodeId::from_u128(11);
    let mut p1 = Props::new();
    p1.insert("name".into(), PropValue::Str("ada".into()));
    let mut p2 = Props::new();
    p2.insert(
        "content".into(),
        PropValue::Str("fixture memory about databases".into()),
    );
    db.submit(vec![
        Op::CreateNode {
            id: n1,
            scope: Scope::Id(s),
            label: "Entity".into(),
            props: p1,
        },
        Op::CreateNode {
            id: n2,
            scope: Scope::Id(s),
            label: "Memory".into(),
            props: p2,
        },
    ])
    .unwrap();
    db.submit(vec![Op::SetEmbedding {
        id: n2,
        model: "m1".into(),
        vector: vec![1.0, 0.0],
    }])
    .unwrap();

    assert!(
        path.exists(),
        "regenerate_fixture: fixture file was not created at {path:?}"
    );
}

#[test]
fn v1_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.redb");
    std::fs::copy(&src, &path).unwrap(); // never open the committed file read-write
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
    let db = Db::open_with(&path, spec).unwrap();
    let s = ScopeId::from_u128(1);
    let scopes = ScopeSet::of(&[s]);
    assert_eq!(
        db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("ada".into()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(db.search_text(&scopes, "databases", 10).unwrap().len(), 1);
    assert_eq!(
        db.search_vector(&VectorQuery {
            scopes: scopes.clone(),
            model: "m1".into(),
            vector: vec![1.0, 0.0],
            k: 1,
            candidates: None,
        })
        .unwrap()
        .len(),
        1
    );
    assert_eq!(db.current_seq().unwrap(), 3);
}
