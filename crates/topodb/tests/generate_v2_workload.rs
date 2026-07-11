//! One-shot frozen v2 migration corpus; run explicitly before v3 format cutover.
use topodb::workload::{batches, WorkloadSpec};
use topodb::{Db, IndexSpec, PropIndex};
#[test]
#[ignore]
fn generate_v2_workload_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v2-workload.redb");
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
    let db = Db::open_with(&path, spec).unwrap();
    for batch in batches(&WorkloadSpec {
        memories: 200,
        ..Default::default()
    }) {
        db.submit(batch).unwrap();
    }
}
