//! Crate-level fixpoint test: ingest → seed → re-ingest must be a pure
//! no-op (zero Created/Superseded/Deduplicated, zero errors). This is the
//! point of the whole feature — a seeded vault must round-trip losslessly.

use topodb::{Db, Scope};
use topodb_json::scopes_to_scope_set;
use topodb_obsidian::{ingest_vault, seed_vault, select_by_entity};

#[test]
fn ingest_seed_ingest_is_a_fixpoint() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), topodb_json::default_spec()).unwrap();
    let lookup = scopes_to_scope_set(&[Scope::Shared]);

    // Ingest the fixture vault (copied to a tempdir so stamping doesn't dirty the repo).
    let src = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vault"));
    let work = tempfile::tempdir().unwrap();
    for p in topodb_obsidian::walk_vault(src).unwrap() {
        let rel = p.strip_prefix(src).unwrap();
        if let Some(parent) = work.path().join(rel).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::copy(&p, work.path().join(rel)).unwrap();
    }
    let r1 = ingest_vault(&db, work.path(), Scope::Shared, &lookup, 1, false, None).unwrap();
    assert_eq!(r1.errors.len(), 1, "bad-yaml.md only");
    assert_eq!(r1.ingested, 3);

    // Seed everything around TokenService into a fresh vault.
    let memories = select_by_entity(&db, &lookup, "TokenService", 2).unwrap();
    assert!(!memories.is_empty());
    let seeded = tempfile::tempdir().unwrap();
    let s = seed_vault(&db, &lookup, seeded.path(), &memories, false).unwrap();
    assert!(s.seeded >= 2 && s.errors.is_empty());

    // Re-ingest the seeded vault untouched: zero new ops of any kind.
    let r2 = ingest_vault(&db, seeded.path(), Scope::Shared, &lookup, 2, false, None).unwrap();
    assert_eq!(
        (r2.ingested, r2.superseded, r2.deduplicated, r2.errors.len()),
        (0, 0, 0, 0),
        "seeded vault must re-ingest as pure skips, got {r2:?}"
    );
    assert_eq!(r2.skipped, s.seeded + s.stubs);
}
