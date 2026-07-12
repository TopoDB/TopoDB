//! Storage open/write benchmarks. Run with `cargo bench -p topodb --bench storage`.
//! `traverse_warm_10k`/`traverse_cold_10k` are Task 13 (BENCHMARKS.md v3)
//! additions. The large-scale open-time gate is NOT here: a criterion bench
//! would have to rebuild its whole fixture as setup on every run, which
//! doesn't fit a bounded command budget at gate scale — it lives in
//! `tests/size_report.rs` as the resumable `build_open_fixture` /
//! `open_report` pair instead (see that module's doc comment).
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use topodb::workload::{batches, WorkloadSpec};
use topodb::{
    Db, Direction, IndexSpec, NodeId, PropIndex, PropValue, ScopeId, ScopeSet, TraversalQuery,
};
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
            memories: 10_000,
            ..Default::default()
        }) {
            db.submit(b).unwrap();
        }
    }
    c.bench_function("cold_open_10k", |b| {
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
/// Builds a 10k-memory workload fixture (default `WorkloadSpec`: embed_pct
/// 20, matching the v1/v2 baseline workload) once, and resolves the seed
/// node for `traverse_warm_10k`/`traverse_cold_10k` via the equality index
/// already declared by `spec()` — entity-0, which `workload::batches` wires
/// roughly 1.5 memories/entity worth of ABOUT/MENTIONS edges into (2,000
/// entities over 10k memories), so a k=2 `Both` traversal from it reaches a
/// nontrivial subgraph (asserted below, matching `recall.rs`'s
/// `seeded_db` sanity check).
fn traversal_fixture() -> (tempfile::TempDir, std::path::PathBuf, ScopeSet, NodeId) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("traverse.redb");
    let db = Db::open_with(&path, spec()).unwrap();
    for b in batches(&WorkloadSpec {
        memories: 10_000,
        ..Default::default()
    }) {
        db.submit(b).unwrap();
    }
    let scopes = ScopeSet::of(&[ScopeId::from_u128(1)]);
    let hits = db
        .nodes_by_prop(
            &scopes,
            "Entity",
            "name",
            &PropValue::Str("entity-0".into()),
        )
        .unwrap();
    let seed = hits[0].id;
    drop(db);
    (dir, path, scopes, seed)
}

fn traverse_warm(c: &mut Criterion) {
    let (_dir, path, scopes, seed) = traversal_fixture();
    let db = Db::open_with(&path, spec()).unwrap();
    let q = TraversalQuery {
        scopes,
        seeds: vec![seed],
        max_hops: 2,
        edge_types: None,
        direction: Direction::Both,
        as_of: None,
    };
    let sanity = db.traverse(&q).unwrap();
    assert!(
        sanity.nodes.len() > 1,
        "traverse_warm_10k fixture must reach more than the seed itself"
    );
    c.bench_function("traverse_warm_10k", |b| b.iter(|| db.traverse(&q).unwrap()));
}

/// Cold = a fresh `Db::open_with` (paying redb's open/vector-index-scan cost)
/// immediately before each single traversal — contrast with `traverse_warm`,
/// which opens once and repeats the traversal on one live handle.
fn traverse_cold(c: &mut Criterion) {
    let (_dir, path, scopes, seed) = traversal_fixture();
    let q = TraversalQuery {
        scopes,
        seeds: vec![seed],
        max_hops: 2,
        edge_types: None,
        direction: Direction::Both,
        as_of: None,
    };
    // Each iteration pays a full cold open, an order of magnitude more
    // expensive than the warm in-process traversal above — a smaller sample
    // count keeps the run in the same ballpark as `cold_open_10k` (also
    // criterion-default 100 samples, ~150-200ms/iter) without ballooning it.
    let mut group = c.benchmark_group("cold");
    group.sample_size(30);
    group.bench_function("traverse_cold_10k", |b| {
        b.iter(|| {
            let db = Db::open_with(&path, spec()).unwrap();
            db.traverse(&q).unwrap()
        })
    });
    group.finish();
}

criterion_group!(benches, cold_open, write, traverse_warm, traverse_cold);
criterion_main!(benches);
