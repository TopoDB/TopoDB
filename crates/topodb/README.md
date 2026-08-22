# topodb

**The memory terrain for AI agents — embedded, temporal, graph-native.**

TopoDB is an embedded, local-first memory engine for AI agents, written in
pure Rust: a scoped temporal property graph on [redb], with an op-log write
path, disk-resident MVCC reads, and k-hop temporal traversal — running
in-process, no server.

Status: **0.1 — breaking changes are 0.2.0.** On-disk format still migrates
in place. The engine core **and the recall layer** are implemented: op log,
single-applier concurrency, scoped temporal traversal (point-in-time `as_of`
and Allen-style valid-time interval predicates), BM25 full-text search,
graph-scoped vector search, hybrid RRF recall (recency-, access-, and
corroboration-weighted), access stats, change feed, and replay-determinism
property tests.

```rust,no_run
use topodb::{
    Db, Direction, IndexSpec, NodeId, Op, PropIndex, PropValue, Scope, ScopeId,
    ScopeSet, TimeAxis, TraversalQuery, VectorQuery,
};

fn main() -> Result<(), topodb::TopoError> {
    // Declare what gets indexed up front (Db::open defaults to NO indexes;
    // the CLI/MCP layers declare this same spec for you automatically).
    let spec = IndexSpec {
        equality: vec![PropIndex { label: "Entity".into(), prop: "name".into() }],
        text: vec![PropIndex { label: "Memory".into(), prop: "content".into() }],
    };
    let db = Db::open_with("memory.topodb", spec)?;
    let scope = ScopeId::new();
    let (a, b) = (NodeId::new(), NodeId::new());

    // Every mutation is a batch of ops, applied atomically.
    db.submit(vec![
        Op::CreateNode {
            id: a,
            scope: Scope::Id(scope),
            label: "Memory".into(),
            props: [(
                "content".to_string(),
                PropValue::Str("ada wrote the first program".into()),
            )]
            .into(),
        },
        Op::CreateNode {
            id: b,
            scope: Scope::Shared,
            label: "Entity".into(),
            props: Default::default(),
        },
        Op::CreateEdge {
            id: Default::default(),
            scope: Scope::Id(scope),
            ty: "ABOUT".into(),
            from: a,
            to: b,
            props: Default::default(),
            valid_from: None,
            recorded_at: None,
        },
        // Embeddings are host-computed and submitted as ops (engine, not policy).
        Op::SetEmbedding { id: a, model: "my-embedder".into(), vector: vec![0.1, 0.2, 0.3] },
    ])?;

    // Every read is scoped — there is no unscoped read path.
    let scopes = ScopeSet::of(&[scope]).with_shared();

    // BM25 full-text recall.
    let _hits = db.search_text(&scopes, "first program", 10)?;

    // Vector recall, scoped, under the same model namespace.
    let _near = db.search_vector(&VectorQuery {
        scopes: scopes.clone(),
        model: "my-embedder".into(),
        vector: vec![0.1, 0.2, 0.3],
        k: 10,
        candidates: None,
    })?;

    // Scoped k-hop temporal traversal.
    let _sub = db.traverse(&TraversalQuery {
        scopes,
        seeds: vec![a],
        max_hops: 2,
        edge_types: None,
        direction: Direction::Out,
        as_of: None,
        time_axis: TimeAxis::Valid,
    })?;
    Ok(())
}
```

## Design principles

1. **Narrow and deep** — an agent-memory engine, not a general graph database
2. **Format stability is a feature** — versioned on-disk format, migrations always
3. **Honest benchmarks** from day one
4. **Engine, not policy** — no LLM calls inside the database, ever
5. **Embedded-first** — servers and sync are future layers, never prerequisites

## Core properties

- **Temporal edges** — facts supersede, never overwrite; `as_of` reads see history, and Allen-style interval predicates (`ValidInterval`: `During`/`Overlaps`/`Before`/`After`) ask interval-vs-interval questions over edge valid time (`edges_from_interval` / `edges_to_interval` / `traverse_interval`)
- **Structural scoping** — every read takes a `ScopeSet`; cross-scope edges require a `Shared` endpoint
- **Deterministic replay** — the op log stores fully-resolved ops; replaying it reproduces state exactly (property-tested)
- **Single-applier concurrency** — writers from any thread serialize through one applier; reads run in redb MVCC read transactions, never block each other, and never block redb's storage commits, though a long-running read can briefly delay the applier's next batch while it holds the dicts/scope-registry read guards

License: MIT OR Apache-2.0.

[redb]: https://crates.io/crates/redb
