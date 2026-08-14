//! Write-path characterization.
//!
//! Criterion times whole operations and cannot see inside a `submit_at`, so
//! phases are recovered by subtraction rather than instrumentation:
//!
//!   index maintenance = (default_spec run) - (empty spec run), same corpus
//!   per-submit fixed cost = the slope across batch sizes
//!
//! This keeps `crates/topodb/src/` unmodified. Op-log append and state apply
//! are NOT separable this way; that would need engine instrumentation and is
//! deliberately deferred.
//!
//! CORRECTION vs. the original task brief: `topodb_json::default_spec()`
//! text-indexes `(label = "Memory", prop = "content")`, not `(label =
//! "Memory", prop = "note")`. The label was right; the prop name was not.
//! The multi-word note text below is written into the `"content"` prop (the
//! `NodeSpec.note` Rust field name is kept as-is — it's just the in-memory
//! field name, not the on-disk prop key) so that Task 2's indexes-on run
//! actually exercises BM25 instead of silently indexing nothing.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use topodb::{Db, EdgeId, NodeId, Op, PropValue, Props, Scope, ScopeId};

/// Fixed vocabulary so BM25 postings are reproducible across runs.
const WORDS: [&str; 8] = [
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
];
const EDGE_TYPES: [&str; 3] = ["RELATES_TO", "MENTIONS", "DERIVED_FROM"];

pub struct NodeSpec {
    pub id: usize,
    pub name: String,
    pub kind: String,
    pub note: String,
    pub rank: i64,
    pub active: bool,
}

pub struct EdgeSpec {
    pub from: usize,
    pub to: usize,
    pub ty: &'static str,
}

/// Deterministic corpus. Uses a small LCG rather than the `rand` crate so the
/// bench has no dependency the engine does not already carry, and so the
/// sequence is stable regardless of `rand` version.
fn corpus(seed: u64, nodes: usize) -> (Vec<NodeSpec>, Vec<EdgeSpec>) {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };

    let node_specs: Vec<NodeSpec> = (0..nodes)
        .map(|id| NodeSpec {
            id,
            name: format!("node-{id}"),
            kind: WORDS[next() % WORDS.len()].to_string(),
            // Multi-word note so BM25 has real postings to maintain.
            note: format!(
                "{} {} {} for node {id}",
                WORDS[next() % WORDS.len()],
                WORDS[next() % WORDS.len()],
                WORDS[next() % WORDS.len()],
            ),
            rank: (next() % 1000) as i64,
            active: next() % 2 == 0,
        })
        .collect();

    // Backward edges: acyclic and every reference resolves by construction.
    // Deduplicated on (from, to, ty) so repeated draws do not inflate the
    // write volume in a way that varies between runs.
    let mut edge_specs = Vec::new();
    for id in 1..nodes {
        let count = 1 + next() % 3;
        let mut mine: Vec<EdgeSpec> = Vec::new();
        let mut attempts = 0;
        while mine.len() < count && attempts < count * 8 {
            attempts += 1;
            let to = next() % id;
            let ty = EDGE_TYPES[next() % EDGE_TYPES.len()];
            if mine.iter().any(|e| e.to == to && e.ty == ty) {
                continue;
            }
            mine.push(EdgeSpec { from: id, to, ty });
        }
        edge_specs.extend(mine);
    }

    (node_specs, edge_specs)
}

/// Load the corpus, submitting `batch` ops per `submit_at` call.
///
/// `batch == 0` means "all in one call". Timestamps are explicit and
/// strictly increasing; every node is written before any edge, since edges
/// reference nodes that must already exist.
fn load(db: &Db, scope: Scope, nodes: &[NodeSpec], edges: &[EdgeSpec], batch: usize) {
    let ids: Vec<NodeId> = nodes.iter().map(|_| NodeId::new()).collect();
    let chunk = if batch == 0 { usize::MAX } else { batch };
    let mut t: i64 = 1;

    let node_ops: Vec<Op> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let mut props = Props::new();
            props.insert("name".into(), PropValue::Str(n.name.clone()));
            props.insert("kind".into(), PropValue::Str(n.kind.clone()));
            // Indexed prop for `default_spec()`'s (Memory, content) text
            // index (see module doc correction above) — NOT "note".
            props.insert("content".into(), PropValue::Str(n.note.clone()));
            props.insert("rank".into(), PropValue::Int(n.rank));
            props.insert("active".into(), PropValue::Bool(n.active));
            Op::CreateNode { id: ids[i], scope, label: "Memory".into(), props }
        })
        .collect();

    for group in node_ops.chunks(chunk.max(1)) {
        db.submit_at(group.to_vec(), t).expect("node batch commits");
        t += 1;
    }

    let edge_ops: Vec<Op> = edges
        .iter()
        .map(|e| Op::CreateEdge {
            id: EdgeId::new(),
            scope,
            ty: e.ty.into(),
            from: ids[e.from],
            to: ids[e.to],
            props: Props::new(),
            valid_from: Some(t),
        })
        .collect();

    for group in edge_ops.chunks(chunk.max(1)) {
        db.submit_at(group.to_vec(), t).expect("edge batch commits");
        t += 1;
    }
}

/// Corpus sizes. 10k is where the full matrix stays fast enough to iterate;
/// 100k (~700k facts) matches the largest size the head-to-head completed, so
/// these numbers are directly comparable to what motivated this work.
const SIZES: [usize; 2] = [10_000, 100_000];
const BATCHES: [usize; 5] = [1, 100, 1_000, 10_000, 0]; // 0 == all in one call

fn batch_label(b: usize) -> String {
    if b == 0 { "all".to_string() } else { b.to_string() }
}

fn bench_empty_spec(c: &mut Criterion) {
    let mut g = c.benchmark_group("write_path_empty_spec");
    // Loading 700k facts takes seconds; the default 100 samples would run for
    // hours. These numbers are for phase attribution, not for detecting a 1%
    // regression, so a small sample is the right trade.
    g.sample_size(10);

    for size in SIZES {
        let (nodes, edges) = corpus(20260719, size);
        for batch in BATCHES {
            let id = format!("n{}/batch{}", size, batch_label(batch));
            g.bench_function(&id, |b| {
                b.iter_batched(
                    || tempfile::tempdir().expect("tempdir"),
                    |dir| {
                        let db = Db::open(dir.path().join("w.redb")).expect("open");
                        let scope = Scope::Id(ScopeId::new());
                        load(&db, scope, &nodes, &edges, batch);
                    },
                    BatchSize::PerIteration,
                )
            });
        }
    }
    g.finish();
}

criterion_group!(benches, bench_empty_spec);
criterion_main!(benches);
