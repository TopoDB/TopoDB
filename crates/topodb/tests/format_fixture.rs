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
//! likewise never re-run now that the v3 cutover has happened.
//!
//! `v3.redb` was the native (un-migrated) v3 fixture, last regenerated during
//! Task 5 of the storage-format-v4 plan (pinning THIS branch's v3 layout,
//! already dual-writing the v4 `vectors`/`embedding_ref` tables — see
//! `generate_v3_workload.rs`'s doc comment on the same "v3 with v4
//! dual-writes already present" distinction). It is now a FIFTH frozen
//! fixture: `FORMAT_VERSION` is 4 everywhere on this branch (Task 7), so no
//! build here can stamp a fresh file at 3 anymore. Its former generator was
//! `regenerate_v3_fixture`; that function is gone (repurposed below as
//! `regenerate_v4_fixture`, targeting `v4.redb` with the identical content
//! recipe) rather than kept around unreachable — `v3.redb`'s committed bytes
//! are untouched by this change and stay frozen at their current content
//! going forward, same as `v1.redb`/`v2.redb`/`v2-workload.redb`.
//!
//! `v3-legacy.redb` (Task 7, storage-format-v4 plan amendment 2 — CORPUS
//! PURITY) is a SIXTH frozen fixture, recovered byte-for-byte from commit
//! `a3711eb` via `git show a3711eb:crates/topodb/tests/fixtures/v3.redb`
//! (2760704 bytes — verified at recovery time and re-verifiable any time via
//! `git show a3711eb:... | wc -c`). It predates Task 3's `vectors`/
//! `embedding_ref` dual-write entirely, so — unlike every other v3-labeled
//! fixture — it has NEITHER of those tables NOR `vector_dims` on disk at
//! all: this is the load-bearing corpus-purity fixture, the only one in the
//! repo that proves `migrate_v3_to_v4` correctly handles v4 tables that are
//! ABSENT going in (every other v3-labeled fixture already carries them,
//! dual-written by a build from Tasks 3/5 onward, and exercises the
//! ALREADY-POPULATED path instead — see `migrate_v4.rs`'s module doc comment
//! on idempotency). A migration bug that forgot to create/populate those
//! tables from scratch would pass every other fixture in this file and only
//! be caught here. Never regenerable (recovered from history, not produced by
//! any build on this branch) and never to be deleted or byte-modified.
//!
//! `v3-workload.redb` (`generate_v3_workload.rs`/`v3_workload_fixture.rs`, a
//! SEPARATE pair of files from this one) is a SEVENTH frozen fixture — a
//! larger, 200-memory v3 corpus (also with v4 dual-writes already present,
//! same distinction as `v3.redb` above) captured before Task 6 rewired
//! postings maintenance to the chunked v4 layout, so it is the one corpus
//! that exercises `migrate_v3_to_v4`'s postings-rechunking pass at a scale
//! where a term's postings split across multiple chunks. Its one-shot
//! `#[ignore]`d generator (`generate_v3_workload_fixture`) is likewise never
//! re-run now that no build on this branch can write single-row-per-term v3
//! postings again.
//!
//! `v4.redb` is the ONLY fixture in this file this build can actually
//! regenerate: it pins the CURRENT native v4 layout (Task 8, storage-format-v4
//! plan) — `regenerate_v4_fixture` writes it using the SAME content recipe
//! `v3.redb` was regenerated with (fixed ids, two nodes, one embedding), which
//! now lands as a native v4 file with no migration involved, since
//! `Storage::open_with`/`Db::open_with` always stamp a brand-new file at the
//! current `FORMAT_VERSION` (4).
//!
//! - `regenerate_v4_fixture` (`#[ignore]`): (re)writes `v4.redb`. Run with
//!   `cargo test -p topodb --test format_fixture -- --ignored regenerate`.
//! - `v1_fixture_opens_and_reads` / `v2_fixture_opens_and_reads` /
//!   `v3_fixture_opens_and_reads` / `v3_legacy_fixture_migrates_and_reads` /
//!   `v4_fixture_opens_and_reads` (run in the normal suite): each copies its
//!   committed fixture to a tempdir and asserts the documented queries
//!   (`nodes_by_prop`, `search_text`, `search_vector`, `current_seq`,
//!   `format_version`) still see the expected content, now (for v1/v2/v3/
//!   v3-legacy) migrated all the way to `FORMAT_VERSION == 4`, or (for v4)
//!   opened natively with no migration at all. The committed file is NEVER
//!   opened read-write in place — `open_with` can write to META
//!   (format_version/index_spec stamping), which would dirty the committed
//!   bytes on every test run.
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
//! Format v5 (equality-index key normalization) changed no table layout:
//! every fixture below now migrates/opens to `FORMAT_VERSION == 5`, with the
//! v4->v5 step being a version stamp plus the `ensure_index_spec`-driven
//! PROP_INDEX rebuild (see `prop_index.rs`'s `PROP_INDEX_NORM_VERSION`).
//! `v4.redb` is therefore FROZEN now (an EIGHTH frozen fixture, the v4
//! migration-input file) — no build on this branch can stamp a fresh file at
//! 4 anymore. Its old generator is repurposed below as
//! `regenerate_v5_fixture` (same content recipe, targeting `v5.redb`),
//! exactly the same generator-repurposing move the v3->v4 flip made.
//!
//! Format v6 (F9-11 Task 7) adds exactly one derived table, `LABEL_INDEX`
//! (`(label_id, scope_id, node_id) -> slot`), built by `migrate_v6::
//! migrate_v5_to_v6` — a single NODES scan, no other table touched — so
//! every fixture below now migrates/opens to `FORMAT_VERSION == 6`.
//! `v5.redb` is therefore FROZEN now (a NINTH frozen fixture, the v5
//! migration-input file) — no build on this branch can stamp a fresh file at
//! 5 anymore. Its old generator is repurposed below as
//! `regenerate_v6_fixture` (same content recipe, targeting `v6.redb`), the
//! same generator-repurposing move every earlier version flip made.
//!
//! Format v7 (F8, HNSW) adds exactly two derived tables, `HNSW_META`/
//! `HNSW_LINKS`, plus one META stamp (`"hnsw_params"`) — see `hnsw.rs` and
//! `storage.rs::ensure_hnsw_params`. No existing table's layout changes, and
//! HNSW graphs build lazily (threshold-triggered), so there is no data pass
//! on migration — every fixture below now migrates/opens to the CURRENT
//! `FORMAT_VERSION` (v7's own hop is stamp-only; see the v8/v9 paragraphs
//! below for the hops that follow it). `v6.redb` is therefore FROZEN now (a
//! TENTH frozen fixture, the v6 migration-input file) — no build on this
//! branch can stamp a fresh file at 6 anymore. Its old generator is
//! repurposed below as `regenerate_v7_fixture` (same content recipe,
//! targeting `v7.redb`), the same generator-repurposing move every earlier
//! version flip made — `regenerate_v7_fixture` has since itself been
//! repurposed again into `regenerate_v8_fixture`, see the v8 paragraph.
//!
//! Format v8 (F8 SQ8 tail, T8) changes no table's set or keying — only the
//! `vectors` table's VALUE encoding, from framed postcard `Vec<f32>` to
//! framed postcard `(f32, Vec<i8>) = (scale, codes)`, applied migration-time
//! by `migrate_v8::quantize_vectors`, plus the `"hnsw_params"` stamp's
//! version bump (2 -> 3, since HNSW scoring moves to integer `cosine_q`
//! over the new codes) — see `FORMAT.md`'s "SQ8 quantization" section for
//! the full change. `v7.redb` is therefore FROZEN now (an ELEVENTH frozen
//! fixture, the v7 migration-input file) — no build on this branch can stamp
//! a fresh file at 7 anymore. Its old generator (`regenerate_v7_fixture`) was
//! repurposed as `regenerate_v8_fixture` (same content recipe, targeting
//! `v8.redb`), the same generator-repurposing move every earlier version
//! flip made.
//!
//! Format v9 (bi-temporal edges) rewrites every EDGES row (`EdgeRecordDiskV3`
//! -> `EdgeRecordDiskV4`, copy-rule backfill: `recorded_at := valid_from`,
//! `superseded_at := valid_to`) and every stored `CreateEdge`/`CloseEdge` op
//! (belief axis backfilled the same way — see `migrate_v9.rs`'s module doc
//! comment) in place. `v8.redb` — written back when `regenerate_v8_fixture`
//! still targeted `v8.redb`, and carrying NO edges at all (see that
//! generator's recipe) — is therefore FROZEN now (a TWELFTH frozen fixture,
//! the v8 migration-input file): no build on this branch can stamp a fresh
//! file at 8 anymore, AND it has no edge rows to exercise the copy-rule
//! backfill against. `v8_fixture_migrates_to_v9_and_reads` below documents
//! this gap explicitly and asserts the strongest available coverage (version
//! and standard queries); the copy-rule backfill itself is exercised by
//! `migrate_v9.rs`'s own unit tests plus this file's native
//! `v9_fixture_opens_and_reads`/`regenerate_v9_fixture`, whose recipe adds
//! one open backdated edge and one closed edge specifically so the NEXT
//! format bump has real edge coverage to migrate. `regenerate_v8_fixture` is
//! repurposed below as `regenerate_v9_fixture` (same base content recipe
//! plus the two new edges, targeting `v9.redb`), the same generator-
//! repurposing move every earlier version flip made. `v9.redb` is now the
//! ONLY fixture this build can regenerate.
//!
//! Node/scope ids use `NodeId::from_u128`/`ScopeId::from_u128`
//! (`#[doc(hidden)]` debug-seam constructors added in `ids.rs` for exactly
//! this purpose) rather than `Ulid::new()`, so the fixture's *content* is
//! reproducible across regenerations. The raw `.redb` bytes are NOT
//! guaranteed byte-for-byte stable (redb's on-disk layout has padding/free
//! space that isn't part of the content contract) — only the query results
//! below are.

use topodb::*;

/// Regenerate with: cargo test -p topodb --release --test format_fixture --
/// --ignored regenerate_v9_fixture --nocapture
///
/// Repurposed from the old `regenerate_v8_fixture` (itself repurposed from
/// `regenerate_v7_fixture`, itself from `regenerate_v6_fixture`, itself from
/// `regenerate_v5_fixture`, itself from the Task 5-era
/// `regenerate_v3_fixture`): same base content recipe, now targeting
/// `v9.redb` — this build's `FORMAT_VERSION` is 9 everywhere, so
/// `Db::open_with` below stamps a native v9 file with no migration involved.
/// `v8.redb` is untouched by this function and stays frozen (see the module
/// doc comment).
///
/// PLUS (Task 2, bi-temporal edges): one open, backdated edge (`e1`,
/// `n1 -> n2`, `valid_from` explicitly set to a timestamp earlier than the
/// write instant, via `submit_at`) and one edge that is created then closed
/// (`e2`, `n2 -> n1`) — each write instant distinct and fixed via
/// `submit_at` so `recorded_at`/`superseded_at` are reproducible and
/// deliberately diverge from `valid_from`/`valid_to` where the recipe
/// intends. This gives the NEXT format bump genuine edge rows to migrate,
/// closing the gap `v8.redb` has (see the module doc comment on
/// `v8_fixture_migrates_to_v9_and_reads`).
#[test]
#[ignore]
fn regenerate_v9_fixture() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v9.redb");
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
        let e1 = EdgeId::from_u128(20);
        let e2 = EdgeId::from_u128(21);
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
        // e1: open, backdated — valid_from explicitly 1_000 (world time), but
        // WRITTEN (recorded_at) at 5_000, so recorded_at != valid_from: a
        // genuine late-recorded fact, not just a copy-rule identity.
        db.submit_at(
            vec![Op::CreateEdge {
                id: e1,
                scope: Scope::Id(s),
                ty: "about".into(),
                from: n1,
                to: n2,
                props: Props::new(),
                valid_from: Some(1_000),
                recorded_at: None, // resolve_op always stamps this; caller input ignored.
            }],
            5_000,
        )
        .unwrap();
        // e2: created at 2_000 (valid_from defaults to the write instant),
        // then closed at 9_000 (valid_to defaults to the close instant) — a
        // plain create-then-close pair with no backdating, distinct write
        // instants from e1's so the fixture exercises more than one value.
        db.submit_at(
            vec![Op::CreateEdge {
                id: e2,
                scope: Scope::Id(s),
                ty: "cites".into(),
                from: n2,
                to: n1,
                props: Props::new(),
                valid_from: None,
                recorded_at: None,
            }],
            2_000,
        )
        .unwrap();
        db.submit_at(
            vec![Op::CloseEdge {
                id: e2,
                valid_to: None,
                superseded_at: None,
            }],
            9_000,
        )
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
        "regenerate_v9_fixture: fixture file was not created at {path:?}"
    );
}

/// The frozen v5 fixture's migration to v6 (F9-11 Task 7's load-bearing
/// migration test): opens the committed `v5.redb` and asserts it lands on
/// `FORMAT_VERSION == 6` with every query — including `nodes_by_label`,
/// which now also proves `LABEL_INDEX` was built correctly by
/// `migrate_v6::migrate_v5_to_v6` (a single NODES scan) — still seeing
/// exactly the fixture recipe's known corpus: one `Entity` node (`n1`, named
/// "ada") and one `Memory` node (`n2`, with embedded text/vector content).
/// Same standard query set `v1`/`v2`/`v3`/`v3-legacy` fixtures use, plus the
/// v5-specific case/whitespace-insensitive equality behavior this fixture
/// already pinned before the v6 bump.
#[test]
fn v5_fixture_migrates_to_v6_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v5.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v5.redb");
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
    assert_eq!(
        db.nodes_by_prop_normalized(&scopes, "Entity", "name", &PropValue::Str(" ADA ".into()))
            .unwrap()
            .len(),
        1,
        "v5 normalized equality keys must match case/whitespace variants"
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
    assert_eq!(
        db.nodes_by_label(&scopes, "Entity").len(),
        1,
        "recipe's known corpus: exactly one Entity node (n1)"
    );
    assert_eq!(
        db.nodes_by_label(&scopes, "Memory").len(),
        1,
        "recipe's known corpus: exactly one Memory node (n2)"
    );
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
}

/// Task 7 (format v6, now FROZEN by the v7 flip): the native v6 fixture,
/// written by the generator now named `regenerate_v7_fixture` back when it
/// still targeted `v6.redb` — same content recipe every native fixture in
/// this file uses. Since `FORMAT_VERSION` is 7 on this branch, opening this
/// fixture now takes the `Some(6)` stamp-only migration hop (see
/// `storage.rs`'s version-dispatch match), not a native open — kept as its
/// own test (name unchanged) so a regression that only breaks THIS
/// fixture's specific migration path is still caught here, in addition to
/// `v6_fixture_migrates_to_v7_and_reads` below.
#[test]
fn v6_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v6.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.redb");
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
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
}

/// The frozen v6 fixture's migration all the way to v8 (F8's load-bearing
/// migration test, mirroring `v5_fixture_migrates_to_v6_and_reads` above):
/// opens the committed `v6.redb` and asserts it lands on the CURRENT
/// `FORMAT_VERSION` with every standard query — including
/// `search_vector` for the fixture's known embedding, now scored via
/// `cosine_q` over SQ8 codes — still seeing exactly the fixture recipe's
/// known corpus. v7's own hop adds no data pass (HNSW graphs build lazily),
/// so this test's real job is proving the `Some(6)` stamp-only hop runs,
/// followed by v8's `migrate_v8::quantize_vectors` data pass, and nothing
/// regresses across either.
#[test]
fn v6_fixture_migrates_to_v7_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v6.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.redb");
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
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
    // The fixture's single `m1` embedding is below `build_threshold` (HNSW's
    // default is far above 1 vector), so it must stay on the scan path: no
    // graph rows at all, in either table.
    assert!(db.debug_dump_hnsw_meta().unwrap().is_empty());
    assert!(db.debug_dump_hnsw_links().unwrap().is_empty());
}

/// The frozen v7 fixture's migration to v8 (F8 SQ8's load-bearing migration
/// test, mirroring `v6_fixture_migrates_to_v7_and_reads` above): opens the
/// committed `v7.redb` — written back when `regenerate_v7_fixture` still
/// targeted `v7.redb` at the then-current `FORMAT_VERSION`, so this fixture
/// carries genuine pre-SQ8 `Vec<f32>` vector rows going in — and asserts it
/// lands on the CURRENT `FORMAT_VERSION` with every standard query, including
/// `search_vector` for the fixture's known embedding, now scored via
/// `cosine_q` over SQ8 codes rather than raw f32 dot/sqrt. Unlike v7's own
/// HNSW-table addition, the v7->v8 hop DOES do a data pass
/// (`migrate_v8::quantize_vectors` rewrites every `vectors` row's VALUE
/// encoding from `Vec<f32>` to `(scale, Vec<i8>)`), so this test's real job
/// is proving that pass runs correctly and the fixture's single embedding
/// still round-trips through quantization well enough for `search_vector`
/// to find it. See `v8_fixture_opens_and_reads` below for the equivalent
/// coverage on a fixture that never carried pre-SQ8 rows at all.
#[test]
fn v7_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v7.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v7.redb");
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
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
    // Same sub-threshold scenario as the v6->v7 migration test above: one
    // `m1` embedding, well under `build_threshold`, so no graph gets built.
    assert!(db.debug_dump_hnsw_meta().unwrap().is_empty());
    assert!(db.debug_dump_hnsw_links().unwrap().is_empty());
}

/// Verbatim copy of `crate::quant::quantize`/`crate::quant::dequantize`
/// (format v8 SQ8 kernel) — copied, NOT imported (the module is
/// `pub(crate)`, unreachable from this external test crate anyway), so this
/// fixture's expected embedding is computed the same way `differential.rs`'s
/// oracle computes its expected values: independently of the engine, from
/// the same documented formula.
fn quantize(v: &[f32]) -> (f32, Vec<i8>) {
    let mut maxabs = 0.0f32;
    for &x in v {
        let a = x.abs();
        if a > maxabs {
            maxabs = a;
        }
    }
    if maxabs == 0.0 || !maxabs.is_finite() {
        return (0.0, vec![0i8; v.len()]);
    }
    let s = 127.0f32 / maxabs;
    let codes = v
        .iter()
        .map(|&x| ((x * s).round() as i32).clamp(-127, 127) as i8)
        .collect();
    (maxabs, codes)
}

fn dequantize(scale: f32, codes: &[i8]) -> Vec<f32> {
    codes.iter().map(|&c| c as f32 * scale / 127.0).collect()
}

/// The frozen v8 fixture's migration to v9 (Task 2, bi-temporal edges'
/// load-bearing migration test, mirroring `v7_fixture_opens_and_reads`
/// above): opens the committed `v8.redb` — written back when
/// `regenerate_v8_fixture` still targeted `v8.redb`, same content recipe
/// every native fixture in this file used (two nodes, one embedding) — and
/// asserts it lands on `FORMAT_VERSION == 9` with every standard query,
/// including a direct check on the stored embedding
/// (`debug_dump_vectors`/`quantize`/`dequantize`, same as the old
/// `v8_fixture_opens_and_reads` this test replaces).
///
/// **Edge-coverage gap, and why it's fine:** `v8.redb`'s recipe has NO
/// edges (verified directly against the generator above) — so this fixture
/// cannot exercise `migrate_v9::bitemporalize`'s EDGES copy-rule backfield
/// field-by-field the way the module doc comment's ideal migration test
/// would. The strongest assertion available here is that the migration
/// completes and `debug_dump_edges` stays empty (proving the EDGES pass
/// runs on a zero-row table without erroring, not that it backfills
/// correctly). The copy-rule backfill itself — field values, not just
/// "doesn't crash" — is covered by `migrate_v9.rs`'s own unit tests
/// (`bitemporalize_backfills_edges_and_rewrites_ops_in_place`, decoding a
/// hand-built V3 row against exact expected V4 output) and by this file's
/// native `v9_fixture_opens_and_reads` below, whose recipe carries a real
/// open backdated edge and a real closed edge specifically so later
/// migrations (and this one, retroactively, via the unit tests) have
/// concrete edge content to check against.
#[test]
fn v8_fixture_migrates_to_v9_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v8.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v8.redb");
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
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
    // Same sub-threshold scenario as v7's native test above: one `m1`
    // embedding, well under `build_threshold`, so no graph gets built.
    assert!(db.debug_dump_hnsw_meta().unwrap().is_empty());
    assert!(db.debug_dump_hnsw_links().unwrap().is_empty());
    // Dequantized-embedding expectation: the fixture's single `vectors` row
    // must dequantize to EXACTLY what independently quantizing/dequantizing
    // the recipe's known `[1.0, 0.0]` embedding produces.
    let rows = db.debug_dump_vectors().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "recipe's known corpus: exactly one embedding"
    );
    let (scale, codes) = quantize(&[1.0f32, 0.0]);
    let expected = dequantize(scale, &codes);
    assert_eq!(
        rows[0].3, expected,
        "stored embedding must dequantize to exactly dequantize(quantize([1.0, 0.0]))"
    );
    // Edge-coverage gap (see doc comment): the migration's EDGES pass must
    // run cleanly over a zero-row table, and stay empty afterward.
    assert!(
        db.debug_dump_edges().is_empty(),
        "v8.redb's recipe has no edges — the v8->v9 EDGES pass has nothing to backfill here"
    );
}

/// Task 2 (bi-temporal edges): the native v9 fixture, written by
/// `regenerate_v9_fixture` directly at the current `FORMAT_VERSION` — no
/// migration involved on open at all, unlike `v8_fixture_migrates_to_v9_and_reads`
/// above. Same standard query set as every native fixture test in this file,
/// plus the load-bearing edge checks the v8 fixture can't provide: `e1` is
/// open and backdated (`valid_from` 1_000, `recorded_at` 5_000 — a genuine
/// late-recorded fact, not the copy-rule identity case) and `e2` was created
/// then closed with no backdating (`valid_from`/`recorded_at` both 2_000,
/// `valid_to`/`superseded_at` both 9_000 — the common case where both axes
/// move together). These are the exact values `regenerate_v9_fixture`'s
/// recipe writes via `submit_at`, so this test also pins that recipe against
/// silent drift.
#[test]
fn v9_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v9.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v9.redb");
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
    let n1 = NodeId::from_u128(10);
    let n2 = NodeId::from_u128(11);
    let e1 = EdgeId::from_u128(20);
    let e2 = EdgeId::from_u128(21);
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
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(db.current_seq().unwrap(), 6);
    assert_eq!(db.format_version(), 9);
    assert!(db.debug_dump_hnsw_meta().unwrap().is_empty());
    assert!(db.debug_dump_hnsw_links().unwrap().is_empty());
    let rows = db.debug_dump_vectors().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "recipe's known corpus: exactly one embedding"
    );
    let (scale, codes) = quantize(&[1.0f32, 0.0]);
    let expected = dequantize(scale, &codes);
    assert_eq!(
        rows[0].3, expected,
        "stored embedding must dequantize to exactly dequantize(quantize([1.0, 0.0]))"
    );

    // e1: open, backdated. World time (valid_from) 1_000, belief time
    // (recorded_at) 5_000 — deliberately divergent.
    let from_n1 = db.edges_from(&scopes, n1, None, None, false).unwrap();
    assert_eq!(
        from_n1.len(),
        1,
        "recipe's known corpus: exactly one edge from n1"
    );
    let edge1 = &from_n1[0];
    assert_eq!(edge1.id, e1);
    assert_eq!(edge1.to, n2);
    assert_eq!(edge1.valid_from, 1_000);
    assert_eq!(edge1.valid_to, None, "e1 is open");
    assert_eq!(edge1.recorded_at, 5_000);
    assert_eq!(edge1.superseded_at, None, "e1 was never closed");

    // e2: created then closed, both axes moving together (no backdating).
    let from_n2 = db.edges_from(&scopes, n2, None, None, false).unwrap();
    assert_eq!(
        from_n2.len(),
        1,
        "recipe's known corpus: exactly one edge from n2"
    );
    let edge2 = &from_n2[0];
    assert_eq!(edge2.id, e2);
    assert_eq!(edge2.to, n1);
    assert_eq!(edge2.valid_from, 2_000);
    assert_eq!(edge2.valid_to, Some(9_000));
    assert_eq!(edge2.recorded_at, 2_000);
    assert_eq!(edge2.superseded_at, Some(9_000));
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
    // F9-11 Task 8: pins that the multi-hop v1->v6 migration chain produces
    // a correct, NON-EMPTY LABEL_INDEX — not just that `nodes_by_label`
    // happens to still work via some other path. Recipe's known corpus:
    // exactly one Entity node (n1) and one Memory node (n2), so exactly one
    // LABEL_INDEX row each, and exactly two rows total on disk.
    assert_eq!(
        db.nodes_by_label(&scopes, "Entity").len(),
        1,
        "v1->v6 migration: recipe's known corpus has exactly one Entity node (n1)"
    );
    assert_eq!(
        db.nodes_by_label(&scopes, "Memory").len(),
        1,
        "v1->v6 migration: recipe's known corpus has exactly one Memory node (n2)"
    );
    assert_eq!(
        db.debug_dump_label_index().unwrap().len(),
        2,
        "v1->v6 migration: LABEL_INDEX must have exactly one row per node (2 total), not be empty"
    );
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
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
    // F9-11 Task 8: v2->v6 migration must produce a correct, NON-EMPTY
    // LABEL_INDEX (see `v1_fixture_opens_and_reads`'s identical assertion
    // block for the recipe/count rationale).
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(
        db.debug_dump_label_index().unwrap().len(),
        2,
        "v2->v6 migration: LABEL_INDEX must have exactly one row per node (2 total), not be empty"
    );
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
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
    // F9-11 Task 8: v3->v6 migration must produce a correct, NON-EMPTY
    // LABEL_INDEX (see `v1_fixture_opens_and_reads`'s identical assertion
    // block for the recipe/count rationale).
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(
        db.debug_dump_label_index().unwrap().len(),
        2,
        "v3->v6 migration: LABEL_INDEX must have exactly one row per node (2 total), not be empty"
    );
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
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
    // F9-11 Task 8: v3-legacy->v6 migration must produce a correct,
    // NON-EMPTY LABEL_INDEX (see `v1_fixture_opens_and_reads`'s identical
    // assertion block for the recipe/count rationale) — this is the
    // corpus-purity fixture, so this also proves the migration builds
    // LABEL_INDEX correctly even starting from a file with none of the v4
    // vector tables present at all.
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(
        db.debug_dump_label_index().unwrap().len(),
        2,
        "v3-legacy->v6 migration: LABEL_INDEX must have exactly one row per node (2 total), not be empty"
    );
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
}

/// Task 8 (storage-format-v4 plan): the native v4 fixture, written by
/// `regenerate_v4_fixture` (the SAME content recipe every earlier native
/// fixture in this file used) directly at the current `FORMAT_VERSION` — no
/// migration involved on open at all, unlike `v1`/`v2`/`v3`/`v3-legacy` above.
/// Same standard query set as `v3_fixture_opens_and_reads`, so a regression
/// that only breaks a freshly-created v4 file (as opposed to one reached via
/// migration) has a fixture that would catch it.
#[test]
fn v4_fixture_opens_and_reads() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v4.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v4.redb");
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
    // F9-11 Task 8: v4->v6 migration must produce a correct, NON-EMPTY
    // LABEL_INDEX (see `v1_fixture_opens_and_reads`'s identical assertion
    // block for the recipe/count rationale).
    assert_eq!(db.nodes_by_label(&scopes, "Entity").len(), 1);
    assert_eq!(db.nodes_by_label(&scopes, "Memory").len(), 1);
    assert_eq!(
        db.debug_dump_label_index().unwrap().len(),
        2,
        "v4->v6 migration: LABEL_INDEX must have exactly one row per node (2 total), not be empty"
    );
    assert_eq!(db.current_seq().unwrap(), 3);
    assert_eq!(db.format_version(), 9);
}

/// F8 review fix: `ensure_hnsw_params` must decode the stored META
/// `"hnsw_params"` stamp before comparing it byte-wise, exactly like
/// `index_spec_reconcile_decision` decodes `"index_spec"` before comparing
/// it. Sabotaging the stamp with undecodable garbage (mirrors
/// `stale_norm_version_triggers_prop_index_rebuild_on_open` in
/// `prop_index.rs` and `stale_analyzer_version_triggers_fts_rebuild_on_open`
/// in `text_search.rs`, which corrupt OTHER stamps via a raw redb write) must
/// surface as a `TopoError::Encoding` on the next open, not be silently
/// treated as "different params" and trigger a drain+rebuild that papers
/// over the corruption.
#[test]
fn corrupt_hnsw_params_stamp_fails_open_with_encoding_error() {
    use redb::TableDefinition;
    const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let spec = IndexSpec {
        equality: vec![],
        text: vec![],
    };
    {
        let db = Db::open_with(&path, spec.clone()).unwrap();
        drop(db);
    }
    {
        // Sabotage from outside: overwrite the stamp with bytes that are not
        // a valid postcard-encoded `HnswParams`.
        let raw = redb::Database::open(&path).unwrap();
        let tx = raw.begin_write().unwrap();
        {
            // A lone `0xFF` is an incomplete postcard varint (continuation
            // bit set, no following byte) — guaranteed to fail decode as the
            // first field of `HnswParams` (a `u32`), regardless of the
            // struct's exact field list.
            let mut meta = tx.open_table(META).unwrap();
            meta.insert("hnsw_params", [0xFFu8].as_slice()).unwrap();
        }
        tx.commit().unwrap();
    }
    let err = Db::open_with(&path, spec).unwrap_err();
    assert!(
        matches!(err, TopoError::Encoding(ref msg) if msg.contains("hnsw_params")),
        "corrupt hnsw_params stamp must fail open with an Encoding error, got: {err:?}"
    );
}
