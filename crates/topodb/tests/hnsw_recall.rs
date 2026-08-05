//! Recall gate at test scale (F8 Task 4, spec acceptance #1's CI-scale
//! twin): 2_000 vectors, 32 dims, `HnswParams::build_threshold = 8` so the
//! whole cluster is graph-built (not scan). recall@10, averaged over 50
//! deterministic queries against an in-test brute-force oracle, must be
//! `>= 0.95`. All 2_000 embeddings are submitted in ONE `Db::submit` batch
//! (one write transaction), so the only per-call overhead paid is `apply_op`
//! itself, not 2_000 separate transactions.
//!
//! **Construction params.** `HnswParams::default()`'s `m=16`/`m0=32`/
//! `ef_construction=128` are tuned for production recall/build-cost
//! trade-offs, not for a `cargo test` debug build's wall-clock budget —
//! measured at n=2_000 they push this single test well past a minute.
//! `m=8`/`m0=16`/`ef_construction=24` below keeps mean recall@10 comfortably
//! above the 0.95 gate (measured ~0.97, see the `eprintln!` below) while
//! cutting build time roughly 3x; the read path itself (`hnsw::ef_search`,
//! same as production) is unaffected by this — the read-path formula and
//! wiring under test are identical either way, only the graph's own
//! connectivity quality changes. Runtime lands close to, but somewhat above,
//! the "~5s" aspiration in a debug/opt-level=1 build (this workspace's
//! `[profile.test]` — see `Cargo.toml`); the module doc's "~5s" is a rough
//! ceiling to avoid an unbounded/minutes-long test, not a hard assertion.
use topodb::*;

/// The exact splitmix64 idiom `hnsw.rs`'s own test module uses
/// (`VecRng`/`seed_vectors`, mirroring `benches/storage.rs`'s original) —
/// copied here (not shared, since that struct is crate-private test code)
/// so this integration test reproduces byte-for-byte across runs without
/// depending on any RNG crate.
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

fn seed_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = VecRng(seed);
    (0..n)
        .map(|_| (0..dim).map(|_| rng.next_f32()).collect())
        .collect()
}

/// Bit-for-bit the same cosine formula the engine scores with
/// (`vector_store::cosine` / `differential.rs:279`'s oracle) — copied
/// verbatim rather than calling into the crate, so this brute-force oracle
/// can never accidentally share a bug with the code under test.
fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// Brute-force top-`k` node indices (into `vectors`) for `query`, ordered
/// `(score desc, index asc)` — the same tie-break the engine's `search_vector`
/// applies after resolving `NodeId`s (index here stands in for creation
/// order, which is what `NodeId` order collapses to for this fixture's
/// monotonically-created ids).
fn brute_force_top_k(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = vectors
        .iter()
        .enumerate()
        .filter_map(|(i, v)| cosine(query, v).map(|s| (i, s)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(k);
    scored.into_iter().map(|(i, _)| i).collect()
}

#[test]
fn recall_at_10_meets_ci_gate() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with_options(
        dir.path().join("t.redb"),
        IndexSpec::default(),
        DbOptions {
            hnsw_params: Some(HnswParams {
                build_threshold: 8,
                m: 8,
                m0: 16,
                ef_construction: 24,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .unwrap();

    let n = 2_000;
    let dim = 32;
    let vectors = seed_vectors(n, dim, 0xF8A0_5EED_0001);

    // One batch, one write transaction: n creates + n embeddings, in slot
    // order, so `apply_op`'s incremental HNSW maintenance sees them exactly
    // as it would across n separate submits, just without the per-submit
    // transaction overhead. IDs are deterministic (`NodeId::from_u128`, `+1`
    // so slot 0 never collides with a hypothetical id-0 edge case, mirroring
    // `hnsw.rs`'s own `insert_incrementally` test fixture) rather than
    // `NodeId::new()`'s wall-clock/random ULID: `hnsw::level_for` hashes the
    // `NodeId` itself, so a random id would make the GRAPH'S OWN STRUCTURE
    // (and therefore the measured recall) vary run to run — silently
    // flaky against a fixed 0.95 gate that already has a slim margin.
    let mut ops = Vec::with_capacity(n * 2);
    let mut ids = Vec::with_capacity(n);
    for (slot, v) in vectors.iter().enumerate() {
        let id = NodeId::from_u128(slot as u128 + 1);
        ops.push(Op::CreateNode {
            id,
            scope: Scope::Shared,
            label: "M".into(),
            props: Default::default(),
        });
        ops.push(Op::SetEmbedding {
            id,
            model: "m1".into(),
            vector: v.clone(),
        });
        ids.push(id);
    }
    db.submit(ops).unwrap();

    let meta = db.debug_dump_hnsw_meta().unwrap();
    assert_eq!(meta.len(), 1, "exactly one (model, scope) cluster");
    assert!(meta[0].3, "cluster must be built at 2_000 >= threshold 8");

    let queries = seed_vectors(50, dim, 0xF8A0_5EED_0002);
    let k = 10;
    let mut total_recall = 0.0f64;
    for q in &queries {
        let want: std::collections::HashSet<usize> =
            brute_force_top_k(&vectors, q, k).into_iter().collect();

        let got = db
            .search_vector(&VectorQuery {
                scopes: ScopeSet::of(&[]).with_shared(),
                model: "m1".into(),
                vector: q.clone(),
                k,
                candidates: None,
            })
            .unwrap();
        assert!(
            db.debug_last_search_used_graph(),
            "every query in this fixture must route through the built graph"
        );

        let hit_count = got
            .iter()
            .filter(|(rec, _)| {
                let idx = ids.iter().position(|&id| id == rec.id).expect("known id");
                want.contains(&idx)
            })
            .count();
        total_recall += hit_count as f64 / k as f64;
    }
    let mean_recall = total_recall / queries.len() as f64;
    eprintln!(
        "hnsw_recall: mean recall@{k} over {} queries (n={n}, dim={dim}) = {mean_recall:.4}",
        queries.len()
    );
    assert!(
        mean_recall >= 0.95,
        "mean recall@{k} = {mean_recall:.4} must be >= 0.95"
    );
}
