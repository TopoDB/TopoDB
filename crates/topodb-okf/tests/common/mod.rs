//! Shared helpers for the topodb-okf integration tests. Mirrors the
//! fixture/tempdir conventions of `topodb-obsidian/tests`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use topodb::{Db, PropValue, Scope, ScopeSet, TimeAxis};
use topodb_json::{scopes_to_scope_set, ENTITY_LABEL, MEMORY_LABEL};

/// Bundle-relative path prop key — the OKF identity anchor (design §Data
/// model: "path prop ... indexed and resolved via find_by_prop").
pub const PATH_PROP: &str = "path";

/// Open a db with the OKF index spec (must index `(Entity, path)` so ingest's
/// `find_by_prop` path dedup and the tests' `nodes_by_prop` lookups work).
pub fn okf_db(dir: &Path) -> Db {
    Db::open_with(dir.join("t.redb"), topodb_okf::okf_spec()).unwrap()
}

pub fn shared_lookup() -> ScopeSet {
    scopes_to_scope_set(&[Scope::Shared])
}

pub fn fixture_bundle() -> PathBuf {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/bundle"
    ))
    .to_path_buf()
}

/// Recursively copy `src` into `dst` (ingest stamps `topodb-id` into files, so
/// tests must never point it at the checked-in fixture).
pub fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// Copy the fixture bundle into a fresh `bundle/` under `work`, returning it.
pub fn copied_bundle(work: &Path) -> PathBuf {
    let dst = work.join("bundle");
    copy_tree(&fixture_bundle(), &dst);
    dst
}

/// The single concept entity carrying `path == rel`, or panic.
pub fn concept_by_path(db: &Db, lookup: &ScopeSet, rel: &str) -> topodb::NodeRecord {
    let mut hits = db
        .nodes_by_prop(lookup, ENTITY_LABEL, PATH_PROP, &PropValue::Str(rel.into()))
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one entity with path {rel:?}"
    );
    hits.remove(0)
}

/// True if `entity` has at least one incoming edge from a Memory node (its
/// attached `about` memory).
pub fn has_attached_memory(db: &Db, lookup: &ScopeSet, entity_id: topodb::NodeId) -> bool {
    db.edges_to(lookup, entity_id, None, None, true, TimeAxis::Valid)
        .unwrap()
        .iter()
        .any(|e| {
            db.node(lookup, e.from)
                .map(|n| n.label.as_str() == MEMORY_LABEL)
                .unwrap_or(false)
        })
}
