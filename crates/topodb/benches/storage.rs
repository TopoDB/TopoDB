//! Storage open/write benchmarks. Run with `cargo bench -p topodb --bench storage`.
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
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
fn cold_open(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("open.redb");
    {
        let db = Db::open_with(&path, spec()).unwrap();
        for b in batches(&WorkloadSpec {
            memories: 1_000,
            ..Default::default()
        }) {
            db.submit(b).unwrap();
        }
    }
    c.bench_function("cold_open_1k", |b| {
        b.iter(|| Db::open_with(&path, spec()).unwrap())
    });
}
fn write(c: &mut Criterion) {
    let all = batches(&WorkloadSpec {
        memories: 1_000,
        ..Default::default()
    });
    c.bench_function("submit_1k_workload", |b| {
        b.iter_batched(
            || {
                let d = tempfile::tempdir().unwrap();
                let db = Db::open_with(d.path().join("w.redb"), spec()).unwrap();
                (d, db, all.clone())
            },
            |(_, db, all)| {
                for x in all {
                    db.submit(x).unwrap();
                }
            },
            BatchSize::PerIteration,
        )
    });
}
criterion_group!(benches, cold_open, write);
criterion_main!(benches);
