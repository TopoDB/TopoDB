//! Unit: bundle walk. `walk_bundle` returns concept-page candidates only —
//! it must skip the reserved `log.md`, scratch `_plan.md`/`_skeleton.md`/
//! `INSTRUCTIONS.md`, dotfiles, and the reserved `index.md` (parsed
//! separately, never as a concept) — design §Ingest step 1.

mod common;
use common::fixture_bundle;
use std::collections::BTreeSet;
use topodb_okf::walk_bundle;

#[test]
fn walk_bundle_returns_concepts_and_skips_reserved_and_scratch() {
    let names: BTreeSet<String> = walk_bundle(&fixture_bundle())
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    // Concept pages are present.
    assert!(names.contains("revenue.md"), "{names:?}");
    assert!(names.contains("customers.md"), "{names:?}");
    assert!(names.contains("onboarding.md"), "{names:?}");

    // Reserved / scratch / index / dotfiles are excluded.
    for skipped in [
        "index.md",
        "log.md",
        "_plan.md",
        "INSTRUCTIONS.md",
        ".hidden.md",
    ] {
        assert!(
            !names.contains(skipped),
            "{skipped} must be skipped: {names:?}"
        );
    }
    assert_eq!(names.len(), 3, "exactly the three concept pages: {names:?}");
}

#[test]
fn walk_bundle_errors_on_missing_dir() {
    assert!(walk_bundle(&fixture_bundle().join("does-not-exist")).is_err());
}
