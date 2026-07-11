//! Explicit size report: `cargo test -p topodb --release --test size_report -- --ignored --nocapture`.
use topodb::workload::{batches, WorkloadSpec};
use topodb::{Db, IndexSpec, PropIndex};
fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    }
}
#[test]
#[ignore]
fn size_report() {
    for memories in [1_000usize, 10_000, 100_000] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.redb");
        let db = Db::open_with(&path, spec()).unwrap();
        for batch in batches(&WorkloadSpec {
            memories,
            ..Default::default()
        }) {
            db.submit(batch).unwrap();
        }
        drop(db);
        let db = Db::open_with(&path, spec()).unwrap();
        let report = db.storage_report().unwrap();
        let file = std::fs::metadata(&path).unwrap().len();
        println!("\n== {memories} memories == file: {file}");
        let mut total = 0;
        for r in report {
            println!("{} {} {} {}", r.table, r.rows, r.key_bytes, r.value_bytes);
            total += r.key_bytes + r.value_bytes;
        }
        println!("logical total: {total}");
    }
}
