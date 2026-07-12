//! Committed format fixtures. `v1.redb` and `v2.redb` are FROZEN migration
//! input, each generated once by an earlier version of this crate and never
//! regenerated since: the code path that originally emitted them (a pre-dict,
//! pre-slot v1 layout for `v1.redb`; `FORMAT_VERSION == 2` for `v2.redb`) is
//! either gone or no longer reachable — `Storage::open_with` always stamps a
//! brand-new file at the CURRENT `FORMAT_VERSION` (4), never an older one, so
//! there is no code path left in this crate that can reproduce either file.
//! `v2-workload.redb` (see `generate_v2_workload.rs`/`v2_workload_fixture.rs`)
//! is a third FROZEN fixture — a larger v2 corpus used by `migrate_v3.rs`'s
//! own migration unit tests — whose one-shot `#[ignore]`d generator is
//! likewise never re-run now that the v3 cutover has happened. `v3.redb` is
//! the only fixture in this file this build can actually regenerate: it pins
//! the CURRENT native (un-migrated) v3 layout, still with the pre-Task-6
//! single-row-per-term POSTINGS shape — `regenerate_v3_fixture` targets a
//! build whose `FORMAT_VERSION` stops at 3, which no longer exists on this
//! branch (v4 is native everywhere now), so this test is `#[ignore]`d and
//! will not be re-run again; `v3.redb` stays frozen at its current content
//! going forward, same as `v1.redb`/`v2.redb`.
//!
//! `v3-legacy.redb` (Task 7, storage-format-v4 plan amendment 2 — CORPUS
//! PURITY) is a FOURTH frozen fixture, recovered byte-for-byte from commit
//! `a3711eb` via `git show a3711eb:crates/topodb/tests/fixtures/v3.redb`
//! (2760704 bytes — verified at recovery time and re-verifiable any time via
//! `git show a3711eb:... | wc -c`). It predates Task 3's `vectors`/
//! `embedding_ref` dual-write entirely, so — unlike every other fixture in
//! this file — it has NEITHER of those tables NOR `vector_dims` on disk at
//! all: this is the load-bearing corpus-purity fixture, the only one in the
//! repo that proves `migrate_v3_to_v4` correctly handles v4 tables that are
//! ABSENT going in (every other v3-labeled fixture already carries them,
//! dual-written by a build from Tasks 3/5 onward, and exercises the
//! ALREADY-POPULATED path instead — see `migrate_v4.rs`'s module doc comment
//! on idempotency). A migration bug that forgot to create/populate those
//! tables from scratch would pass every other fixture in this file and only
//! be caught here.
//!
//! - `regenerate_v3_fixture` (`#[ignore]`, frozen — see above): historically
//!   (re)wrote `v3.redb` using the SAME content recipe `v2.redb` was
//!   originally generated with. Retained for provenance; no longer runnable
//!   against this branch's `FORMAT_VERSION`.
//! - `v1_fixture_opens_and_reads` / `v2_fixture_opens_and_reads` /
//!   `v3_fixture_opens_and_reads` / `v3_legacy_fixture_migrates_and_reads`
//!   (run in the normal suite): each copies its committed fixture to a
//!   tempdir and asserts the documented queries (`nodes_by_prop`,
//!   `search_text`, `search_vector`, `current_seq`, `format_version`) still
//!   see the expected content, now migrated all the way to `FORMAT_VERSION
//!   == 4`. The committed file is NEVER opened read-write in place —
//!   `open_with` can write to META (format_version/index_spec stamping),
//!   which would dirty the committed bytes on every test run.
//!
//! Task 7 (format v4) landed `migrate_v3_to_v4`: `v3.redb`/`v3-legacy.redb`
//! now migrate all the way to `FORMAT_VERSION == 4` on open (re-chunking
//! their single-row-per-term POSTINGS into the v4 chunked layout, and — for
//! `v3-legacy.redb` specifically — creating `vectors`/`embedding_ref`/
//! `vector_dims` from scratch, since it predates them). `v1_fixture_opens_
//! and_reads`/`v2_fixture_opens_and_reads`/
//! `v2_workload_fixture_is_readable_before_cutover` chain the same way
//! (`v1`/`v2` -> ... -> `v4`): migrating from `FORMAT_VERSION` 1 or 2 always
//! rebuilds POSTINGS from scratch via `fts_update` (see
//! `migrate_v3::migrate_v2_to_v3`'s doc comment) using THIS build's current
//! (already-chunked) `set_posting`, so those fixtures never touch
//! `migrate_v3_to_v4`'s postings-rechunking pass at all (`postings_already_
//! chunked = true` — see `migrate_v4.rs`'s module doc comment).
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
fn regenerate_v3_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3.redb");
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
    {
        let db = Db::open_with(&path, spec).unwrap();
        // Fixed ids so the fixture's CONTENT is reproducible across
        // regenerations (the raw bytes are not — see the module doc comment).
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
        // `db` drops here: `Drop for Inner` joins the applier/bumper threads
        // and closes the redb file handle, so the raw reopen+compact below
        // gets exclusive access.
    }

    // redb pre-allocates generously as a file grows; a handful of tiny write
    // transactions leaves several MB of free space committed to disk. Compact
    // before shipping the fixture so the checked-in binary reflects only the
    // handful of rows above, not incidental redb bookkeeping overhead.
    {
        let mut raw = redb::Database::open(&path).unwrap();
        raw.compact().unwrap();
    }

    assert!(
        path.exists(),
        "regenerate_v3_fixture: fixture file was not created at {path:?}"
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
    assert_eq!(db.format_version(), 4);
    drop(db);
    // The second open takes the v4 fast path; migration is idempotent.
    let reopened = Db::open_with(
        &path,
        IndexSpec {
            equality: vec![PropIndex {
                label: "Entity".into(),
                prop: "name".into(),
            }],
            text: vec![PropIndex {
                label: "Memory".into(),
                prop: "content".into(),
            }],
        },
    )
    .unwrap();
    assert_eq!(reopened.current_seq().unwrap(), 3);
}

#[test]
fn v2_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.redb");
    std::fs::copy(&src, &path).unwrap();
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
    let scopes = ScopeSet::of(&[ScopeId::from_u128(1)]);
    assert_eq!(
        db.nodes_by_prop(&scopes, "Entity", "name", &PropValue::Str("ada".into()))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(db.search_text(&scopes, "databases", 10).unwrap().len(), 1);
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 4);
}

/// Migrates all the way to v4: the mid-branch `#[ignore]` guard lifted now
/// that `migrate_v3_to_v4` (Task 7) re-chunks `v3.redb`'s single-row-per-term
/// POSTINGS into the v4 layout on open.
#[test]
fn v3_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.redb");
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
    let scopes = ScopeSet::of(&[ScopeId::from_u128(1)]);
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
    assert_eq!(db.format_version(), 4);
}

/// The load-bearing migration test (Task 7, corpus-purity amendment): a
/// GENUINE pre-v4 v3 file, recovered from `a3711eb` — no `vectors`/
/// `embedding_ref`/`vector_dims` tables on disk at all, unlike every other
/// v3-labeled fixture in this file (all dual-written by a build from Tasks
/// 3/5 onward). Proves `migrate_v3_to_v4`'s vectors pass creates and
/// populates those tables from scratch, not just idempotently re-writes rows
/// already there — a migration bug that forgot the `CREATE`/populate path
/// entirely would pass every other fixture here and only be caught by this
/// one. Same query set as `v3_fixture_opens_and_reads`, same content (the
/// recovery is byte-for-byte the same source data), same post-migration
/// version.
#[test]
fn v3_legacy_fixture_migrates_and_reads() {
    let src =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3-legacy.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3-legacy.redb");
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
    let scopes = ScopeSet::of(&[ScopeId::from_u128(1)]);
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
    assert_eq!(db.format_version(), 4);
}
