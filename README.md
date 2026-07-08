# TopoDB

**The memory terrain for AI agents — embedded, temporal, graph-native.**

TopoDB is an embedded, local-first memory engine for AI agents, written in
pure Rust: a property graph with temporal facts (facts supersede, never
overwrite), scope-aware recall, graph-scoped vector search, and a change feed
for external consolidation — running in-process, no server.

Status: **early development (0.0.x)** — the engine core works (op-log write
path, single-applier concurrency, scoped k-hop temporal traversal,
replay-determinism property tests); the recall layer (vector search,
full-text, change feed) is next. API not yet stable. Design spec:
[docs/superpowers/specs/2026-07-08-topodb-design.md](docs/superpowers/specs/2026-07-08-topodb-design.md).

First consumer: Atlas (agentic OS desktop app).

## Principles

1. Narrow and deep — one workload done excellently
2. Format stability is a feature — versioned on-disk format, migrations always
3. Honest benchmarks from day one
4. Engine, not policy — no LLM calls inside the database, ever
5. Embedded-first — servers and sync are future layers, never prerequisites
