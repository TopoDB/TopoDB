//! Seed — graph → OKF bundle. Asserts design §Seed: concept entities render as
//! `.md` pages under their stored `path`, frontmatter is reconstructed
//! (nested `generated`/`verified`/`sources`, `tags` re-split to a list),
//! reserved `index.md` (root carries `okf_version`) and `log.md` are
//! generated, and writes are non-clobbering.

mod common;
use common::{copied_bundle, okf_db, shared_lookup};
use serde_yaml::Value as Yaml;
use topodb::Scope;
use topodb_obsidian::select_by_entity;
use topodb_okf::{ingest_okf, seed_okf, Note};

fn y(s: &str) -> Yaml {
    Yaml::String(s.into())
}

/// Ingest the fixture, select the working set around "Revenue", return the db
/// handle (kept alive) plus the selected memories.
fn ingested_db(work: &std::path::Path) -> (topodb::Db, Vec<topodb::NodeRecord>) {
    let bundle = copied_bundle(work);
    let db = okf_db(work);
    let lookup = shared_lookup();
    ingest_okf(&db, &bundle, Scope::Shared, &lookup, 1, false, None).unwrap();
    let mems = select_by_entity(&db, &lookup, "Revenue", 2).unwrap();
    assert!(!mems.is_empty(), "selection must find concept memories");
    (db, mems)
}

#[test]
fn seed_reconstructs_nested_provenance_and_resplit_tags() {
    let work = tempfile::tempdir().unwrap();
    let (db, mems) = ingested_db(work.path());
    let lookup = shared_lookup();
    let out = tempfile::tempdir().unwrap();

    let s = seed_okf(&db, &lookup, &mems, out.path(), false, false).unwrap();
    assert!(s.errors.is_empty(), "{s:?}");

    // The stored `path` prop is reused for placement (design §Seed path/filename).
    let text = std::fs::read_to_string(out.path().join("metrics/revenue.md"))
        .expect("revenue seeded at its stored path");
    let note = Note::parse(&text).unwrap();
    let fm = &note.frontmatter;

    assert_eq!(fm.get(y("type")).and_then(|v| v.as_str()), Some("metric"));
    assert_eq!(fm.get(y("title")).and_then(|v| v.as_str()), Some("Revenue"));
    assert_eq!(fm.get(y("status")).and_then(|v| v.as_str()), Some("stable"));
    assert_eq!(
        fm.get(y("stale_after")).and_then(|v| v.as_str()),
        Some("2026-12-31")
    );

    // tags: flattened "finance, kpi" prop re-split back to a YAML list.
    let tags: Vec<&str> = fm
        .get(y("tags"))
        .and_then(|v| v.as_sequence())
        .expect("tags re-split to a sequence")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(tags, vec!["finance", "kpi"]);

    // generated: {by, at} rebuilt from the actor edge.
    let gen = fm
        .get(y("generated"))
        .and_then(|v| v.as_mapping())
        .expect("generated nested map");
    assert_eq!(
        gen.get(y("by")).and_then(|v| v.as_str()),
        Some("openwiki/0.3")
    );
    assert_eq!(
        gen.get(y("at")).and_then(|v| v.as_str()),
        Some("2026-08-01T00:00:00Z")
    );

    // verified: [{by, at}] rebuilt as a one-element list.
    let ver = fm
        .get(y("verified"))
        .and_then(|v| v.as_sequence())
        .expect("verified list");
    assert_eq!(ver.len(), 1);
    let v0 = ver[0].as_mapping().unwrap();
    assert_eq!(v0.get(y("by")).and_then(|v| v.as_str()), Some("human:drew"));

    // sources: [{resource, author, ...}] rebuilt from source + author edges.
    let srcs = fm
        .get(y("sources"))
        .and_then(|v| v.as_sequence())
        .expect("sources list");
    assert_eq!(srcs.len(), 1);
    let s0 = srcs[0].as_mapping().unwrap();
    assert_eq!(
        s0.get(y("resource")).and_then(|v| v.as_str()),
        Some("https://example.com/report.pdf")
    );
    assert_eq!(
        s0.get(y("author")).and_then(|v| v.as_str()),
        Some("analyst/1.0")
    );

    // topodb-id stamped as the identity anchor.
    assert!(note.id().is_some(), "seed stamps topodb-id");
}

#[test]
fn seed_writes_root_index_with_okf_version() {
    let work = tempfile::tempdir().unwrap();
    let (db, mems) = ingested_db(work.path());
    let lookup = shared_lookup();
    let out = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems, out.path(), false, false).unwrap();

    let index =
        std::fs::read_to_string(out.path().join("index.md")).expect("root index.md generated");
    let note = Note::parse(&index).unwrap();
    assert_eq!(
        note.frontmatter
            .get(y("okf_version"))
            .and_then(|v| v.as_str()),
        Some("0.2"),
        "root index carries the okf_version marker"
    );
}

#[test]
fn seed_with_log_writes_log_md_and_without_omits_it() {
    let work = tempfile::tempdir().unwrap();
    let (db, mems) = ingested_db(work.path());
    let lookup = shared_lookup();

    let with = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems, with.path(), true, false).unwrap();
    assert!(
        with.path().join("log.md").exists(),
        "with_log writes log.md"
    );

    let without = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems, without.path(), false, false).unwrap();
    assert!(
        !without.path().join("log.md").exists(),
        "with_log=false omits log.md"
    );
}

#[test]
fn seed_is_non_clobbering_unless_overwrite() {
    let work = tempfile::tempdir().unwrap();
    let (db, mems) = ingested_db(work.path());
    let lookup = shared_lookup();
    let out = tempfile::tempdir().unwrap();

    seed_okf(&db, &lookup, &mems, out.path(), false, false).unwrap();
    let page = out.path().join("metrics/revenue.md");

    // A local edit is protected by a non-overwrite re-seed.
    std::fs::write(&page, "local edit\n").unwrap();
    let guarded = seed_okf(&db, &lookup, &mems, out.path(), false, false).unwrap();
    assert!(
        guarded.skipped >= 1,
        "edited page must be skipped: {guarded:?}"
    );
    assert_eq!(std::fs::read_to_string(&page).unwrap(), "local edit\n");

    // overwrite=true clobbers it back to the generated form.
    let forced = seed_okf(&db, &lookup, &mems, out.path(), false, true).unwrap();
    assert!(
        forced.seeded >= 1,
        "overwrite re-seeds the page: {forced:?}"
    );
    assert!(std::fs::read_to_string(&page)
        .unwrap()
        .contains("type: metric"));
}
