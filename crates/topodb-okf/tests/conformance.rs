//! Conformance — OKF §11 "tolerant consumer". A bundle is never rejected for
//! missing optional fields, an unknown/free-form `type`, unknown extra
//! frontmatter keys, or a missing `index.md`; a bare `verified` mapping is
//! read as a one-element list. Unknown keys survive a round-trip (design
//! §Principles "Tolerant consumer" + §Round-trip).

mod common;
use common::{okf_db, shared_lookup};
use serde_yaml::Value as Yaml;
use topodb::Scope;
use topodb_obsidian::select_by_entity;
use topodb_okf::{ingest_okf, seed_okf, Note};

fn y(s: &str) -> Yaml {
    Yaml::String(s.into())
}

/// Write a single-page bundle (no index.md) into a fresh tempdir.
fn one_page_bundle(name: &str, contents: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(name), contents).unwrap();
    dir
}

#[test]
fn accepts_missing_index_and_missing_optional_fields() {
    let bundle = one_page_bundle(
        "thing.md",
        "---\ntype: whatever\ntitle: Thing\n---\nJust a body, no optionals.\n",
    );
    let work = tempfile::tempdir().unwrap();
    let db = okf_db(work.path());
    let lookup = shared_lookup();

    // No index.md, no description/tags/status/provenance — still fine.
    let r = ingest_okf(&db, bundle.path(), Scope::Shared, &lookup, 1, false, None).unwrap();
    assert_eq!(r.ingested, 1, "{r:?}");
    assert!(r.errors.is_empty(), "{r:?}");
}

#[test]
fn free_form_type_is_never_validated() {
    // `type` is an open vocabulary — an unusual value must not error.
    let bundle = one_page_bundle(
        "oddity.md",
        "---\ntype: some-made-up-type\ntitle: Oddity\n---\nBody.\n",
    );
    let work = tempfile::tempdir().unwrap();
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    let r = ingest_okf(&db, bundle.path(), Scope::Shared, &lookup, 1, false, None).unwrap();
    assert_eq!((r.ingested, r.errors.len()), (1, 0), "{r:?}");
}

#[test]
fn bare_verified_mapping_is_treated_as_one_element_list() {
    // `verified` given as a bare mapping (not a list) — OKF §5.2 tolerance.
    let bundle = one_page_bundle(
        "solo.md",
        concat!(
            "---\n",
            "type: metric\n",
            "title: Solo\n",
            "verified:\n",
            "  by: human:kim\n",
            "  at: \"2026-08-10T00:00:00Z\"\n",
            "---\n",
            "Body.\n",
        ),
    );
    let work = tempfile::tempdir().unwrap();
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    ingest_okf(&db, bundle.path(), Scope::Shared, &lookup, 1, false, None).unwrap();

    // The reviewer actor was still promoted (bare mapping read as 1-elem list).
    assert!(
        topodb_json::find_existing_entity(&db, &lookup, "human:kim")
            .unwrap()
            .is_some(),
        "bare verified mapping must promote its actor"
    );

    // On seed, `verified` comes back as a one-element list.
    let mems = select_by_entity(&db, &lookup, "Solo", 2).unwrap();
    let out = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems, out.path(), false, false).unwrap();
    let note = Note::parse(&std::fs::read_to_string(out.path().join("solo.md")).unwrap()).unwrap();
    let ver = note
        .frontmatter
        .get(y("verified"))
        .and_then(|v| v.as_sequence())
        .expect("verified normalized to a list");
    assert_eq!(ver.len(), 1);
    assert_eq!(
        ver[0]
            .as_mapping()
            .unwrap()
            .get(y("by"))
            .and_then(|v| v.as_str()),
        Some("human:kim")
    );
}

#[test]
fn unknown_extension_keys_survive_roundtrip() {
    let bundle = one_page_bundle(
        "ext.md",
        concat!(
            "---\n",
            "type: metric\n",
            "title: Ext\n",
            "weird_scalar: 42\n",
            "custom_field:\n",
            "  nested_key: nested_value\n",
            "---\n",
            "Body.\n",
        ),
    );
    let work = tempfile::tempdir().unwrap();
    let db = okf_db(work.path());
    let lookup = shared_lookup();
    ingest_okf(&db, bundle.path(), Scope::Shared, &lookup, 1, false, None).unwrap();

    let mems = select_by_entity(&db, &lookup, "Ext", 2).unwrap();
    let out = tempfile::tempdir().unwrap();
    seed_okf(&db, &lookup, &mems, out.path(), false, false).unwrap();
    let note = Note::parse(&std::fs::read_to_string(out.path().join("ext.md")).unwrap()).unwrap();
    let fm = &note.frontmatter;

    // Long-tail scalar preserved (dotted-key flatten ⇄ reserialize).
    assert_eq!(
        fm.get(y("weird_scalar")).and_then(|v| v.as_i64()),
        Some(42),
        "unknown scalar must survive: {fm:?}"
    );
    // Unknown nested key un-flattened back to a nested mapping.
    let custom = fm
        .get(y("custom_field"))
        .and_then(|v| v.as_mapping())
        .expect("unknown nested key round-trips as a map");
    assert_eq!(
        custom.get(y("nested_key")).and_then(|v| v.as_str()),
        Some("nested_value")
    );
}
