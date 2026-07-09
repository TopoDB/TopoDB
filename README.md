# TopoDB

[![crates.io](https://img.shields.io/crates/v/topodb.svg)](https://crates.io/crates/topodb)
[![docs.rs](https://img.shields.io/docsrs/topodb)](https://docs.rs/topodb)
[![topodb-mcp on crates.io](https://img.shields.io/crates/v/topodb-mcp.svg?label=topodb-mcp)](https://crates.io/crates/topodb-mcp)
[![topodb-mcp on docs.rs](https://img.shields.io/docsrs/topodb-mcp?label=docs.rs%3A%20topodb-mcp)](https://docs.rs/topodb-mcp)

**The memory terrain for AI agents — embedded, temporal, graph-native.**

TopoDB is an embedded, local-first memory engine for AI agents, written in
pure Rust: a property graph with temporal facts (facts supersede, never
overwrite), scope-aware recall, graph-scoped vector search, and a change feed
for external consolidation — running in-process, no server.

Status: **early development (0.0.x)** — the engine core works (op-log write
path, single-applier concurrency, scoped k-hop temporal traversal,
replay-determinism property tests); the recall layer (vector search,
full-text, change feed) is next. API not yet stable.

First consumer: Atlas (agentic OS desktop app).

## Principles

1. Narrow and deep — one workload done excellently
2. Format stability is a feature — versioned on-disk format, migrations always
3. Honest benchmarks from day one
4. Engine, not policy — no LLM calls inside the database, ever
5. Embedded-first — servers and sync are future layers, never prerequisites

## Crates

| Crate | crates.io | What it is |
|---|---|---|
| [`topodb`](crates/topodb) | [![crates.io](https://img.shields.io/crates/v/topodb.svg)](https://crates.io/crates/topodb) | The embedded engine itself — link it into your process as a library. |
| [`topodb-mcp`](crates/topodb-mcp) | [![crates.io](https://img.shields.io/crates/v/topodb-mcp.svg)](https://crates.io/crates/topodb-mcp) | An MCP (Model Context Protocol) server exposing a `topodb` database over stdio, for coding agents and other MCP clients that want scoped recall/write tools without embedding Rust. |

### topodb-mcp

`topodb-mcp` is a standalone binary: point it at a `.redb` file and it serves 10 MCP tools —
`db_info`, six read tools (`get_node`, `find_by_prop`, `search_memories`, `traverse`,
`access_stats`, `get_changes`), and three write tools (`create_memory`, `create_entity`,
`link`) — over stdio JSON-RPC. Install with `cargo install topodb-mcp` and wire it into Claude
Code or Claude Desktop in a couple of lines. See
[`crates/topodb-mcp/README.md`](crates/topodb-mcp/README.md) for the full CLI reference, tool
table, client config examples, and v0 limitations (no vector search yet, create-only writes).
