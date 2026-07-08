# topodb

**The memory terrain for AI agents — embedded, temporal, graph-native.**

TopoDB is an embedded, local-first memory engine for AI agents, written in
pure Rust: a scoped temporal property graph on [redb], with an op-log write
path, lock-free snapshot reads, and k-hop temporal traversal — running
in-process, no server.

Status: **early development (0.0.x)** — the engine core works (op log,
single-applier concurrency, scoped temporal traversal, replay-determinism
property tests), but the API is not yet stable and the recall layer (vector
search, full-text, change feed) is still landing. Not production-ready;
pin exact versions.

```rust
use topodb::{Db, Op, Scope, ScopeId, ScopeSet, NodeId, TraversalQuery, Direction};

let db = Db::open("memory.topodb")?;
let scope = ScopeId::new();
let (a, b) = (NodeId::new(), NodeId::new());

// Every mutation is a batch of ops, applied atomically.
db.submit(vec![
    Op::CreateNode { id: a, scope: Scope::Id(scope), label: "Memory".into(), props: Default::default() },
    Op::CreateNode { id: b, scope: Scope::Shared, label: "Entity".into(), props: Default::default() },
    Op::CreateEdge { id: Default::default(), scope: Scope::Id(scope), ty: "ABOUT".into(),
                     from: a, to: b, props: Default::default(), valid_from: None },
])?;

// Every read is scoped — there is no unscoped read path.
let scopes = ScopeSet::of(&[scope]).with_shared();
let sub = db.traverse(&TraversalQuery {
    scopes, seeds: vec![a], max_hops: 2,
    edge_types: None, direction: Direction::Out, as_of: None,
})?;
```

## Design principles

1. **Narrow and deep** — an agent-memory engine, not a general graph database
2. **Format stability is a feature** — versioned on-disk format, migrations always
3. **Honest benchmarks** from day one
4. **Engine, not policy** — no LLM calls inside the database, ever
5. **Embedded-first** — servers and sync are future layers, never prerequisites

## Core properties

- **Temporal edges** — facts supersede, never overwrite; `as_of` reads see history
- **Structural scoping** — every read takes a `ScopeSet`; cross-scope edges require a `Shared` endpoint
- **Deterministic replay** — the op log stores fully-resolved ops; replaying it reproduces state exactly (property-tested)
- **Single-applier concurrency** — writers from any thread serialize through one applier; readers are lock-free against arc-swapped persistent snapshots

License: MIT OR Apache-2.0.

[redb]: https://crates.io/crates/redb
