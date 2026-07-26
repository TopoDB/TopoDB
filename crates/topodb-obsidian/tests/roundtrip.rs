//! Crate-level fixpoint test: ingest → seed → re-ingest must be a pure
//! no-op (zero Created/Superseded/Deduplicated, zero errors). This is the
//! point of the whole feature — a seeded vault must round-trip losslessly.

use topodb::{Db, Scope, ScopeId};
use topodb_json::scopes_to_scope_set;
use topodb_obsidian::{ingest_vault, note_to_input, plan_note, seed_vault, select_by_entity, Note};

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

/// Regression for the seed/ingest scope asymmetry: an entity created in
/// `Shared` scope, linked from a memory written in a narrower project scope.
/// `seed_vault` must read with the SAME lookup `ingest_vault` uses (project +
/// Shared) so it can see the Shared entity, render the edge, and write the
/// stub — and the untouched seeded vault must then re-ingest as a pure no-op.
/// Before the fix, seeding with a project-only scope set silently dropped the
/// `related:` link and the stub; re-ingesting the untouched note under the
/// wider [project, Shared] lookup then saw the edge and spuriously
/// superseded it, closing the memory's only edge.
#[test]
fn seed_reads_include_shared_scope_for_cross_scope_links() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), topodb_json::default_spec()).unwrap();
    let project = Scope::Id(ScopeId::new());
    let lookup = scopes_to_scope_set(&[project, Scope::Shared]);

    // Entity created in Shared scope first.
    let shared_lookup = scopes_to_scope_set(&[Scope::Shared]);
    let seed_entity = plan_note(
        &db,
        Scope::Shared,
        &shared_lookup,
        1,
        &note_to_input(&Note::parse("Seed fact about [[Redis]].\n").unwrap()).unwrap(),
    )
    .unwrap();
    db.submit(seed_entity.ops).unwrap();

    // Memory created in the narrower project scope, linked to the Shared
    // entity: dedup via the wide lookup finds the existing Shared entity
    // rather than creating a new project-scoped one.
    let mem_out = plan_note(
        &db,
        project,
        &lookup,
        2,
        &note_to_input(&Note::parse("Project fact about [[Redis]].\n").unwrap()).unwrap(),
    )
    .unwrap();
    let topodb_obsidian::NoteAction::Created { memory_id } = mem_out.action else {
        panic!("expected a new memory")
    };
    db.submit(mem_out.ops).unwrap();
    let mem = db.node(&lookup, memory_id).unwrap();

    // Seed with the SAME [project, Shared] lookup: must render the related:
    // link and write the entity stub, not silently drop the edge.
    let vdir = tempfile::tempdir().unwrap();
    let s = seed_vault(&db, &lookup, vdir.path(), std::slice::from_ref(&mem), false).unwrap();
    assert_eq!((s.seeded, s.stubs, s.errors.len()), (1, 1, 0));

    let names: Vec<_> = std::fs::read_dir(vdir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().any(|n| n == "Redis.md"), "{names:?}");
    let note_name = names
        .iter()
        .find(|n| n.starts_with("Project"))
        .unwrap_or_else(|| panic!("expected the project memory note among {names:?}"));
    let text = std::fs::read_to_string(vdir.path().join(note_name)).unwrap();
    assert!(
        text.contains("[[Redis]]"),
        "seeded note must carry the related: link, got:\n{text}"
    );

    // Re-ingest the untouched seeded vault under the same lookup: pure no-op.
    let r2 = ingest_vault(&db, vdir.path(), project, &lookup, 3, false, None).unwrap();
    assert_eq!(
        (r2.ingested, r2.superseded, r2.deduplicated, r2.errors.len()),
        (0, 0, 0, 0),
        "untouched seeded vault must re-ingest as a pure no-op, got {r2:?}"
    );
}
