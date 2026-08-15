//! Round-trip fixpoint — the correctness anchor of the whole feature
//! (design §"Round-trip fixpoint"):
//!
//!   * `ingest → seed → ingest` is a no-op (seeded bundle re-ingests as pure
//!     skips), and
//!   * `seed → ingest → seed` is a no-op, and
//!   * `seed(ingest(bundle))` reproduces the bundle's SEMANTIC content
//!     (frontmatter fields, promoted provenance, body links) — we own
//!     canonical serialization, so equality is semantic, not byte-identical.
//!
//! Contract referenced by every test file (the implementer must expose these
//! at the crate root, mirroring `topodb-obsidian`'s exports):
//!   * `Note` (nested-YAML frontmatter parser)
//!   * `ingest_okf(db, bundle, write_scope, lookup, now_ms, dry_run, embed)
//!        -> Result<IngestReport, String>`
//!   * `seed_okf(db, scopes, selection, bundle_dir, with_log, overwrite)
//!        -> Result<SeedReport, String>`
//!   * `walk_bundle`, `extract_links`, `resolve_link`, `okf_spec`
//!   * `IngestReport`, `SeedReport`, `FileError`

mod common;
use common::{copied_bundle, okf_db, shared_lookup};
use serde_yaml::Value as Yaml;
use topodb::Scope;
use topodb_obsidian::select_by_entity;
use topodb_okf::{ingest_okf, seed_okf, Note};

fn y(s: &str) -> Yaml {
    Yaml::String(s.into())
}

/// Semantic frontmatter equality for a concept page: every field the original
/// carried is reproduced (ignoring the stamped `topodb-id` identity anchor and
/// key ordering — canonical serialization is ours to choose).
fn assert_frontmatter_semantically_equal(original: &Note, seeded: &Note) {
    for (k, v) in &original.frontmatter {
        let key = k.as_str().unwrap();
        if key == "topodb-id" {
            continue;
        }
        let got = seeded
            .frontmatter
            .get(k.clone())
            .unwrap_or_else(|| panic!("seeded page dropped frontmatter key {key:?}"));
        assert_eq!(
            got, v,
            "frontmatter key {key:?} not reproduced: want {v:?}, got {got:?}"
        );
    }
}

#[test]
fn ingest_then_seed_reproduces_semantic_content() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();

    // Capture the original revenue page BEFORE ingest stamps a topodb-id.
    let original =
        Note::parse(&std::fs::read_to_string(bundle.join("metrics/revenue.md")).unwrap()).unwrap();

    ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    let mems = select_by_entity(&db, &lookup, "Revenue", 2).unwrap();
    let seeded_dir = tempfile::tempdir().unwrap();
    let s = seed_okf(&db, &lookup, &mems, seeded_dir.path(), true, false).unwrap();
    assert!(s.errors.is_empty(), "{s:?}");

    let seeded = Note::parse(
        &std::fs::read_to_string(seeded_dir.path().join("metrics/revenue.md")).unwrap(),
    )
    .unwrap();

    // Every original frontmatter field (type/title/description/resource/tags/
    // status/stale_after/generated/verified/sources/runtime/custom_field) is
    // reproduced semantically.
    assert_frontmatter_semantically_equal(&original, &seeded);

    // The body carries the resolved body link back as an absolute-from-root
    // /path.md reference to the customers concept.
    assert!(
        seeded.body.contains("/tables/customers.md"),
        "seeded body must render the references link: {}",
        seeded.body
    );
}

#[test]
fn ingest_seed_ingest_is_a_fixpoint() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();

    let r1 = ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    assert_eq!(r1.ingested, 3, "{r1:?}");

    let mems = select_by_entity(&db, &lookup, "Revenue", 2).unwrap();
    assert!(!mems.is_empty());
    let seeded_dir = tempfile::tempdir().unwrap();
    let s = seed_okf(&db, &lookup, &mems, seeded_dir.path(), true, false).unwrap();
    assert!(s.seeded >= 3 && s.errors.is_empty(), "{s:?}");

    // Re-ingest the untouched seeded bundle: zero new ops of any kind.
    let r2 = ingest_okf(
        &db,
        seeded_dir.path(),
        Scope::Shared,
        &lookup,
        2,
        false,
        None,
    )
    .unwrap();
    assert_eq!(
        (r2.ingested, r2.superseded, r2.deduplicated, r2.errors.len()),
        (0, 0, 0, 0),
        "seeded bundle must re-ingest as pure skips, got {r2:?}"
    );
}

#[test]
fn seed_ingest_seed_is_a_fixpoint() {
    let work = tempfile::tempdir().unwrap();
    let bundle = copied_bundle(work.path());
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    let mems = select_by_entity(&db, &lookup, "Revenue", 2).unwrap();

    // First seed.
    let dir_a = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems, dir_a.path(), true, false).unwrap();
    let revenue_a = std::fs::read_to_string(dir_a.path().join("metrics/revenue.md")).unwrap();

    // Ingest that seeded bundle (untouched) — pure skips.
    let r = ingest_okf(&db, dir_a.path(), Scope::Shared, &lookup, 2, false, None).unwrap();
    assert_eq!(
        (r.ingested, r.superseded, r.deduplicated, r.errors.len()),
        (0, 0, 0, 0),
        "{r:?}"
    );

    // Second seed of the same working set reproduces byte-identical pages
    // (we own serialization, so seed→ingest→seed is stable).
    let mems2 = select_by_entity(&db, &lookup, "Revenue", 2).unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems2, dir_b.path(), true, false).unwrap();
    let revenue_b = std::fs::read_to_string(dir_b.path().join("metrics/revenue.md")).unwrap();
    assert_eq!(revenue_a, revenue_b, "seed must be deterministic/stable");

    // And both are equal-modulo-topodb-id to a re-parse (sanity on the fixpoint).
    let na = Note::parse(&revenue_a).unwrap();
    assert_eq!(
        na.frontmatter.get(y("type")).and_then(|v| v.as_str()),
        Some("metric")
    );
}
