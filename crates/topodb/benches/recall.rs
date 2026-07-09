use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use topodb::*;

/// Nodes per `submit` batch during seeding. Individual per-node submits (each
/// a redb transaction) are too slow for a 10k-node seed; batching amortises
/// the transaction cost while staying legal (same-batch `CreateNode` +
/// `SetEmbedding` pairs resolve against each other, and every embedding in a
/// batch shares `dim`, so dim pre-validation passes).
const SEED_CHUNK: usize = 500;

/// Number of hub spokes (and, symmetrically, second-hop leaves) wired into
/// the seed graph so `traverse_seed_2hop` walks a nonzero subgraph: `ids[0]`
/// (the hub) connects to `HUB_SPOKES` first-hop nodes, each of which connects
/// on to one more second-hop node, for `2 * HUB_SPOKES` edges total.
const HUB_SPOKES: usize = 50;

fn seeded_db(n: usize, dim: usize) -> (tempfile::TempDir, Db, ScopeId, Vec<NodeId>) {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![PropIndex {
            label: "M".into(),
            prop: "k".into(),
        }],
        text: vec![PropIndex {
            label: "M".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("b.redb"), spec).unwrap();
    let s = ScopeId::new();
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        ids.push(NodeId::new());
    }

    for (chunk_idx, chunk) in ids.chunks(SEED_CHUNK).enumerate() {
        let mut ops = Vec::with_capacity(chunk.len() * 2);
        for (offset, &id) in chunk.iter().enumerate() {
            let i = chunk_idx * SEED_CHUNK + offset;
            let mut props = Props::new();
            props.insert("k".into(), PropValue::Str(format!("k{}", i % 64)));
            // Every 100th node's content also carries "needle" — a ~1%-selectivity
            // term used by `search_text_selective_10k` below, contrasted against
            // "memory"/"topic" which (per the format string) hit ~100% of docs.
            let content = if i.is_multiple_of(100) {
                format!("memory number {i} about topic {} needle", i % 17)
            } else {
                format!("memory number {i} about topic {}", i % 17)
            };
            props.insert("content".into(), PropValue::Str(content));
            ops.push(Op::CreateNode {
                id,
                scope: Scope::Id(s),
                label: "M".into(),
                props,
            });
            let mut v = vec![0.0f32; dim];
            v[i % dim] = 1.0;
            ops.push(Op::SetEmbedding {
                id,
                model: "m1".into(),
                vector: v,
            });
        }
        db.submit(ops).unwrap();
    }

    // Hub pattern: ids[0] --(HUB_SPOKES edges)--> ids[1..=HUB_SPOKES]
    // --(HUB_SPOKES edges)--> ids[HUB_SPOKES+1..=2*HUB_SPOKES], so a 2-hop
    // traversal from ids[0] visits ids[0] plus both spoke rings.
    let mut edge_ops = Vec::with_capacity(HUB_SPOKES * 2);
    for i in 1..=HUB_SPOKES {
        edge_ops.push(Op::CreateEdge {
            id: EdgeId::new(),
            scope: Scope::Id(s),
            ty: "LINK".into(),
            from: ids[0],
            to: ids[i],
            props: Default::default(),
            valid_from: None,
        });
        edge_ops.push(Op::CreateEdge {
            id: EdgeId::new(),
            scope: Scope::Id(s),
            ty: "LINK".into(),
            from: ids[i],
            to: ids[HUB_SPOKES + i],
            props: Default::default(),
            valid_from: None,
        });
    }
    db.submit(edge_ops).unwrap();

    // Non-timed sanity check: the hub graph wired above must actually connect
    // something, or `traverse_seed_2hop` below would be silently benchmarking
    // a single-node no-op traversal instead of a real 2-hop walk.
    let sanity = db
        .traverse(&TraversalQuery {
            scopes: ScopeSet::of(&[s]),
            seeds: vec![ids[0]],
            max_hops: 2,
            edge_types: None,
            direction: Direction::Both,
            as_of: None,
        })
        .unwrap();
    assert!(
        sanity.nodes.len() > 1,
        "hub traversal from ids[0] must reach more than itself"
    );

    (dir, db, s, ids)
}

fn bench_recall(c: &mut Criterion) {
    let (_d, db, s, ids) = seeded_db(10_000, 32);
    let scopes = ScopeSet::of(&[s]);

    // Bench ordering note: `submit_create_10` runs first and permanently adds
    // nodes to `db` for the remainder of this `bench_recall` invocation (there
    // is no rollback between `c.bench_function` calls — they share one `db`).
    // That's safe for every bench below because the nodes it creates carry
    // `props: Default::default()` — no `k`/`content` props and no embedding —
    // so they are invisible to `nodes_by_prop_10k` (declared-prop equality
    // index), `search_text_10k`/`search_text_selective_10k` (no declared text,
    // so no postings entry), and `search_vector_10k_dim32` (no `SetEmbedding`,
    // so no slab row). `traverse_seed_2hop` only walks edges from `ids[0]`,
    // which these orphan nodes are never wired into.
    c.bench_function("submit_create_10", |b| {
        b.iter_batched(
            || {
                (0..10)
                    .map(|_| Op::CreateNode {
                        id: NodeId::new(),
                        scope: Scope::Id(s),
                        label: "M".into(),
                        props: Default::default(),
                    })
                    .collect::<Vec<_>>()
            },
            |ops| db.submit(ops).unwrap(),
            BatchSize::SmallInput,
        )
    });
    c.bench_function("nodes_by_prop_10k", |b| {
        b.iter(|| {
            db.nodes_by_prop(&scopes, "M", "k", &PropValue::Str("k7".into()))
                .unwrap()
        })
    });
    c.bench_function("search_vector_10k_dim32", |b| {
        let mut q = vec![0.0f32; 32];
        q[7] = 1.0;
        b.iter(|| {
            db.search_vector(&VectorQuery {
                scopes: scopes.clone(),
                model: "m1".into(),
                vector: q.clone(),
                k: 10,
                candidates: None,
            })
            .unwrap()
        })
    });
    c.bench_function("search_text_10k", |b| {
        b.iter(|| db.search_text(&scopes, "memory topic", 10).unwrap())
    });
    // Contrast with `search_text_10k` above: "memory"/"topic" hit ~100% of the
    // 10k-doc corpus (a worst-case stress query), while "needle" hits only the
    // ~1% of nodes seeded with it (every 100th node — see `seeded_db`). Both
    // numbers together are Plan 5's FTS-optimization go/no-go input.
    c.bench_function("search_text_selective_10k", |b| {
        b.iter(|| db.search_text(&scopes, "needle", 10).unwrap())
    });
    c.bench_function("traverse_seed_2hop", |b| {
        let q = TraversalQuery {
            scopes: scopes.clone(),
            seeds: vec![ids[0]],
            max_hops: 2,
            edge_types: None,
            direction: Direction::Both,
            as_of: None,
        };
        b.iter(|| db.traverse(&q).unwrap())
    });
}

criterion_group!(benches, bench_recall);
criterion_main!(benches);
