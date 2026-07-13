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
    Db, Direction, IndexSpec, NodeId, Op, PropIndex, PropValue, Scope, ScopeId, ScopeSet,
    TraversalQuery, VectorQuery,
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

/// Vectors seeded per `submit` batch while building `vector_fixture` — same
/// rationale as `recall.rs`'s `SEED_CHUNK`: one transaction per node would be
/// too slow for 10k nodes, and every embedding in a batch shares `dim`, so
/// the per-model dimension pin (`storage::check_or_pin_dim`, permanent once
/// a model's first `SetEmbedding` sets it — there is no RAM-slab
/// pre-validation left to consult; that machinery was deleted with the v3
/// index) never rejects a batch, regardless of chunk boundaries.
const VECTOR_SEED_CHUNK: usize = 500;

/// Deterministic splitmix64, mirroring `topodb::workload`'s private
/// generator — used here (not that all-zero-but-one-coordinate approach)
/// specifically so every vector's cosine score against a fixed query is
/// generically distinct. An earlier version of this fixture used one-hot
/// vectors (`v[i % DIM] = 1.0`, else 0); with only `DIM` distinct
/// directions spread across 10k nodes, the vast majority of rows tied
/// EXACTLY on cosine score, which pathologically inflated
/// `push_topk`'s tied-group draining path (a ~950ms warm search for
/// 10k/768-dim, ~100x slower than dense-vector runs) — an artifact of the
/// fixture, not of `search_vector`. Dense random floats (matching
/// `workload::batches`' own embedding generator) avoid that degenerate
/// case and match what a real embedding model actually produces.
struct VecRng(u64);
impl VecRng {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z as f32 / u64::MAX as f32) * 2.0 - 1.0
    }
}

/// Task 9 (v4 gates): one scope holding 10k nodes, every one embedded under
/// model `"bench-768"` at the real 768-dim the design spec's gate table
/// specifies (`docs/superpowers/specs/2026-07-11-storage-format-v4-vectors-design.md`),
/// contrasted with `recall.rs`'s pre-existing `search_vector_10k_dim32`
/// (dim 32, and only some nodes embedded) — that bench predates the v4 gate
/// and stays as its own, unrelated data point. Returns a specific known
/// embedded id (`ids[0]`) for `get_node_embedded`, alongside a query vector
/// (dense, not a fixture member) so `k=10` search does real cosine work
/// against generically-distinct scores.
fn vector_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    ScopeSet,
    NodeId,
    Vec<f32>,
) {
    const DIM: usize = 768;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vec.redb");
    let db = Db::open_with(&path, spec()).unwrap();
    let scope_id = ScopeId::new();
    let scope = Scope::Id(scope_id);
    let mut ids = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        ids.push(NodeId::new());
    }
    let mut r = VecRng(0xC0FFEE);
    for chunk in ids.chunks(VECTOR_SEED_CHUNK) {
        let mut ops = Vec::with_capacity(chunk.len() * 2);
        for &id in chunk {
            ops.push(Op::CreateNode {
                id,
                scope,
                label: "Vec".into(),
                props: Default::default(),
            });
            let v: Vec<f32> = (0..DIM).map(|_| r.next_f32()).collect();
            ops.push(Op::SetEmbedding {
                id,
                model: "bench-768".into(),
                vector: v,
            });
        }
        db.submit(ops).unwrap();
    }
    drop(db);
    let scopes = ScopeSet::of(&[scope_id]);
    let q: Vec<f32> = (0..DIM).map(|_| r.next_f32()).collect();
    (dir, path, scopes, ids[0], q)
}

/// Warm scoped vector search p95 (Task 9 gate 2): repeated `search_vector`
/// calls on one already-open `Db` handle. Criterion's `sample.json` supplies
/// the p95 the same way `traverse_warm_10k` does (see that bench's header
/// comment in BENCHMARKS.md's v3 section).
fn search_warm_10k_scope(c: &mut Criterion) {
    let (_dir, path, scopes, _seed, q) = vector_fixture();
    let db = Db::open_with(&path, spec()).unwrap();
    let query = VectorQuery {
        scopes: scopes.clone(),
        model: "bench-768".into(),
        vector: q.clone(),
        k: 10,
        candidates: None,
    };
    let sanity = db.search_vector(&query).unwrap();
    assert_eq!(
        sanity.len(),
        10,
        "search_warm_10k_scope fixture must return k=10 hits from a 10k-vector scope"
    );
    c.bench_function("search_warm_10k_scope", |b| {
        b.iter(|| db.search_vector(&query).unwrap())
    });
}

/// Cold scoped vector search p95 (Task 9 gate 3, ungated/report-only): a
/// fresh `Db::open_with` immediately before each single search, mirroring
/// `traverse_cold`'s cold/warm contrast and reduced `sample_size` (a cold
/// open dominates the per-iteration cost).
fn search_cold_10k_scope(c: &mut Criterion) {
    let (_dir, path, scopes, _seed, q) = vector_fixture();
    let query = VectorQuery {
        scopes,
        model: "bench-768".into(),
        vector: q,
        k: 10,
        candidates: None,
    };
    let mut group = c.benchmark_group("cold");
    group.sample_size(30);
    group.bench_function("search_cold_10k_scope", |b| {
        b.iter(|| {
            let db = Db::open_with(&path, spec()).unwrap();
            db.search_vector(&query).unwrap()
        })
    });
    group.finish();
}

/// `get_node` on an embedded node (Task 9 gate 4): the v4 read path opens
/// `VECTORS`/`EMBEDDING_REF` in `Storage::load_node` where v3 only opened
/// `EMBEDDINGS` — this is the "one extra point read" the design spec's gate
/// table asks to be checked for a measurable regression. The v3-vs-v4 delta
/// itself is reported in BENCHMARKS.md/the task report from a matched
/// manual-timing harness run against both engine versions (criterion
/// benches aren't comparable across separately-compiled binaries); this
/// bench is the persisted v4-side number.
fn get_node_embedded(c: &mut Criterion) {
    let (_dir, path, scopes, seed, _q) = vector_fixture();
    let db = Db::open_with(&path, spec()).unwrap();
    let sanity = db.node(&scopes, seed);
    assert!(
        sanity.is_some(),
        "get_node_embedded fixture's seed id must resolve"
    );
    c.bench_function("get_node_embedded", |b| {
        b.iter(|| db.node(&scopes, seed).unwrap())
    });
}

criterion_group!(
    benches,
    cold_open,
    write,
    traverse_warm,
    traverse_cold,
    search_warm_10k_scope,
    search_cold_10k_scope,
    get_node_embedded
);
criterion_main!(benches);
