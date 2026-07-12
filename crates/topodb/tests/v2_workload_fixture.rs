//! Guards the frozen v2 corpus used by the v3 chained-migration tests.
use topodb::{Db, IndexSpec, PropIndex, ScopeId, ScopeSet};

#[test]
fn v2_workload_fixture_is_readable_before_cutover() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-workload.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workload.redb");
    std::fs::copy(source, &path).unwrap();
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
    // The full v2 -> v3 -> v4 chain runs on open (Task 7's format flip).
    assert_eq!(db.format_version(), 4);
    assert_eq!(db.current_seq().unwrap(), 772);
    assert!(!db
        .search_text(&scopes, "agent memory", 10)
        .unwrap()
        .is_empty());
}
