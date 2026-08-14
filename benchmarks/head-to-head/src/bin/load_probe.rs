//! Load-path decomposition.
//!
//! The head-to-head shows TopoDB loading ~1.9x slower than minigraf. The
//! earlier batching investigation established that chunking makes it *worse*
//! and concluded "per-`submit_at` fixed cost dominates" -- but that conclusion
//! does not survive arithmetic: the unchunked case issues only TWO commits for
//! the whole corpus, so fsync can account for at most a few tens of ms of a
//! ~3s load. Something else is spending the other ~97%.
//!
//! This drives `topodb::Db` directly rather than through the `Engine` trait so
//! one variable moves at a time: node count, props per node, prop payload
//! size, edge count, and commit count are each independently controllable.
//!
//! Everything uses an empty `IndexSpec` (`Db::open`), matching what the
//! head-to-head driver does -- so no BM25, no equality index. Whatever this
//! measures is the bare write path.

use std::time::{Duration, Instant};

use topodb::{Db, EdgeId, NodeId, Op, PropValue, Props, Scope, ScopeId};

fn scratch(name: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join("topodb-load-probe");
    std::fs::create_dir_all(&base).expect("create scratch");
    let p = base.join(format!("{name}.redb"));
    let _ = std::fs::remove_file(&p);
    p
}

struct Shape {
    nodes: usize,
    props_per_node: usize,
    prop_len: usize,
    edges: usize,
    /// Ops per `submit_at` call. 0 means "one call per kind".
    batch: usize,
}

struct Outcome {
    total: Duration,
    node_phase: Duration,
    edge_phase: Duration,
    commits: usize,
    file_bytes: u64,
}

fn build(name: &str, s: &Shape) -> Outcome {
    let path = scratch(name);
    let db = Db::open(&path).expect("open");
    let scope = Scope::Id(ScopeId::from_u128(0xB0F0));
    let ids: Vec<NodeId> = (0..s.nodes).map(|i| NodeId::from_u128(i as u128)).collect();
    let filler: String = "x".repeat(s.prop_len);
    let mut commits = 0usize;

    let node_ops: Vec<Op> = (0..s.nodes)
        .map(|i| {
            let mut props = Props::new();
            for p in 0..s.props_per_node {
                props.insert(
                    format!("p{p}").into(),
                    PropValue::Str(format!("{filler}{i}")),
                );
            }
            Op::CreateNode {
                id: ids[i],
                scope,
                label: "Memory".into(),
                props,
            }
        })
        .collect();

    let chunk = if s.batch == 0 { usize::MAX } else { s.batch };

    let t0 = Instant::now();
    for group in node_ops.chunks(chunk.max(1)) {
        db.submit_at(group.to_vec(), 1).expect("node batch");
        commits += 1;
    }
    let node_phase = t0.elapsed();

    // Backward edges so every endpoint exists by construction.
    let edge_ops: Vec<Op> = (0..s.edges)
        .map(|e| {
            let from = (e % s.nodes.max(1)).max(1);
            let to = e % from;
            Op::CreateEdge {
                id: EdgeId::from_u128(e as u128),
                scope,
                ty: "RELATES_TO".into(),
                from: ids[from],
                to: ids[to],
                props: Props::new(),
                valid_from: Some(2),
            }
        })
        .collect();

    let t1 = Instant::now();
    for group in edge_ops.chunks(chunk.max(1)) {
        db.submit_at(group.to_vec(), 2).expect("edge batch");
        commits += 1;
    }
    let edge_phase = t1.elapsed();

    let total = t0.elapsed();
    drop(db);
    let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    Outcome {
        total,
        node_phase,
        edge_phase,
        commits,
        file_bytes,
    }
}

fn row(label: &str, s: &Shape, o: &Outcome) {
    let per_node = if s.nodes > 0 {
        o.node_phase.as_secs_f64() * 1e6 / s.nodes as f64
    } else {
        0.0
    };
    let per_edge = if s.edges > 0 {
        o.edge_phase.as_secs_f64() * 1e6 / s.edges as f64
    } else {
        0.0
    };
    println!(
        "{label:<26} total={:>8.3}s  nodes={:>8.3}s ({per_node:>6.1} µs/node)  edges={:>7.3}s ({per_edge:>6.1} µs/edge)  commits={:<5} file={}",
        o.total.as_secs_f64(),
        o.node_phase.as_secs_f64(),
        o.edge_phase.as_secs_f64(),
        o.commits,
        o.file_bytes
    );
}

fn main() {
    let n: usize = std::env::var("PROBE_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let e = n * 2;

    println!("=== load decomposition, {n} nodes / {e} edges, empty IndexSpec ===\n");

    // 1. The baseline shape the head-to-head uses: 5 props, one call per kind.
    let base = Shape {
        nodes: n,
        props_per_node: 5,
        prop_len: 8,
        edges: e,
        batch: 0,
    };
    let o = build("base", &base);
    row("baseline 5 props", &base, &o);

    // 2. Commit count. Only two commits happen above, so if fsync mattered,
    //    forcing many more should dominate -- and if it does not, the earlier
    //    "per-submit fixed cost dominates" reading is wrong.
    for batch in [5_000usize, 1_000, 200] {
        let s = Shape {
            batch,
            ..Shape {
                nodes: n,
                props_per_node: 5,
                prop_len: 8,
                edges: e,
                batch,
            }
        };
        let o = build(&format!("b{batch}"), &s);
        row(&format!("batch={batch}"), &s, &o);
    }

    // 3. Props per node: the slope gives per-prop cost, and tells us whether
    //    cost tracks props (payload) or nodes (per-row overhead).
    for pp in [1usize, 5, 10, 20] {
        let s = Shape {
            nodes: n,
            props_per_node: pp,
            prop_len: 8,
            edges: 0,
            batch: 0,
        };
        let o = build(&format!("pp{pp}"), &s);
        row(&format!("{pp} props, no edges"), &s, &o);
    }

    // 5. Endpoint locality. Edge count is held FIXED while the node set
    //    shrinks, so per-edge work is constant except for how spread out the
    //    two endpoint lookups are. If resolving `from`/`to` (NodeId -> slot,
    //    plus the applier's pre-batch `load_nodes`) is what makes a propless
    //    edge cost more than a 5-prop node, a small hot node set should be
    //    dramatically cheaper per edge. If per-edge cost barely moves, the
    //    cost is the five index tables each edge writes, not the lookups.
    for node_set in [100usize, 1_000, 20_000] {
        let s = Shape {
            nodes: node_set,
            props_per_node: 5,
            prop_len: 8,
            edges: 40_000,
            batch: 0,
        };
        let o = build(&format!("loc{node_set}"), &s);
        row(&format!("40k edges over {node_set} nodes"), &s, &o);
    }

    // 6. What does a node read actually cost? `apply_op`'s `CreateEdge` arm
    //    calls `read_node` twice per edge -- full lz4 + postcard decode, props
    //    map construction, dict/scope resolution, plus two more table lookups
    //    for embeddings -- and uses only `.scope` from each. If two reads
    //    account for most of the ~95 µs/edge, that is the load-path cause.
    {
        use topodb::ScopeSet;
        let path = scratch("readcost");
        let db = Db::open(&path).expect("open");
        let scope = Scope::Id(ScopeId::from_u128(0xB0F0));
        let ids: Vec<NodeId> = (0..n).map(|i| NodeId::from_u128(i as u128)).collect();
        let ops: Vec<Op> = (0..n)
            .map(|i| {
                let mut props = Props::new();
                for p in 0..5 {
                    props.insert(format!("p{p}").into(), PropValue::Str(format!("xxxxxxxx{i}")));
                }
                Op::CreateNode {
                    id: ids[i],
                    scope,
                    label: "Memory".into(),
                    props,
                }
            })
            .collect();
        db.submit_at(ops, 1).expect("nodes");

        let scopes = ScopeSet::of(&[ScopeId::from_u128(0xB0F0)]);
        let reads = 2 * 40_000usize; // what 40k edges would perform
        let t = Instant::now();
        let mut found = 0usize;
        for i in 0..reads {
            if db.node(&scopes, ids[i % n]).is_some() {
                found += 1;
            }
        }
        let d = t.elapsed();
        println!(
            "\n{:<26} {reads} reads in {:.3}s ({:.1} µs/read, {:.1} µs per edge's two reads), found={found}",
            "node read cost",
            d.as_secs_f64(),
            d.as_secs_f64() * 1e6 / reads as f64,
            d.as_secs_f64() * 1e6 / 40_000.0
        );
    }

    // 4. Payload size at fixed prop count. If the op log duplicating each
    //    payload is the cost, this scales steeply with value length.
    for len in [8usize, 64, 512] {
        let s = Shape {
            nodes: n,
            props_per_node: 5,
            prop_len: len,
            edges: 0,
            batch: 0,
        };
        let o = build(&format!("len{len}"), &s);
        row(&format!("5 props x {len}B values"), &s, &o);
    }
}
