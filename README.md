# TopoDB

[![crates.io](https://img.shields.io/crates/v/topodb.svg)](https://crates.io/crates/topodb)
[![docs.rs](https://img.shields.io/docsrs/topodb)](https://docs.rs/topodb)
[![topodb-json on crates.io](https://img.shields.io/crates/v/topodb-json.svg?label=topodb-json)](https://crates.io/crates/topodb-json)
[![topodb-json on docs.rs](https://img.shields.io/docsrs/topodb-json?label=docs.rs%3A%20topodb-json)](https://docs.rs/topodb-json)
[![topodb-mcp on crates.io](https://img.shields.io/crates/v/topodb-mcp.svg?label=topodb-mcp)](https://crates.io/crates/topodb-mcp)
[![topodb-mcp on docs.rs](https://img.shields.io/docsrs/topodb-mcp?label=docs.rs%3A%20topodb-mcp)](https://docs.rs/topodb-mcp)
[![topodb-cli on crates.io](https://img.shields.io/crates/v/topodb-cli.svg?label=topodb-cli)](https://crates.io/crates/topodb-cli)
[![topodb-cli on docs.rs](https://img.shields.io/docsrs/topodb-cli?label=docs.rs%3A%20topodb-cli)](https://docs.rs/topodb-cli)

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
| [`topodb-json`](crates/topodb-json) | [![crates.io](https://img.shields.io/crates/v/topodb-json.svg)](https://crates.io/crates/topodb-json) | The shared JSON↔engine conversion layer used by `topodb-mcp` and `topodb-cli`. Not a library you typically depend on directly. |
| [`topodb-mcp`](crates/topodb-mcp) | [![crates.io](https://img.shields.io/crates/v/topodb-mcp.svg)](https://crates.io/crates/topodb-mcp) | An MCP (Model Context Protocol) server exposing a `topodb` database over stdio, for coding agents and other MCP clients that want scoped recall/write tools without embedding Rust. |
| [`topodb-cli`](crates/topodb-cli) | [![crates.io](https://img.shields.io/crates/v/topodb-cli.svg)](https://crates.io/crates/topodb-cli) | A direct-embedded `topodb` command-line binary — JSON in, JSON out, predictable exit codes — for scripting and ad hoc inspection of a database file without a server or an MCP client. |

### topodb-cli

`topodb-cli` installs a binary named **`topodb`**: point it at a `.redb` file and it gives you
all 11 engine operations (`info`, `create-memory`, `create-entity`, `link`, `get`, `find`,
`search`, `traverse`, `stats`, `changes`, `compact`) as one-shot, script-friendly subcommands —
compact JSON on stdout, a `{"error":{"kind","message"}}` shape on stderr, and exit codes you can
branch on in a shell script (`0` success, `2` rejected/bad input, `1` internal/db-open failure).
It opens the database file directly and in-process, the same way `topodb-mcp` does — no server,
no network hop, and (because of that) no running concurrently with something else that already
has the same file open. Install with `cargo install topodb-cli`. See
[`crates/topodb-cli/README.md`](crates/topodb-cli/README.md) for the full command table,
exit-code contract, scoping rules, and v1 limitations (no vector search, no set-props/remove-node,
no bulk/stdin submit).

### topodb-mcp

`topodb-mcp` is a standalone binary: point it at a `.redb` file and it serves 16 MCP tools over
stdio JSON-RPC — `db_info`; scoped reads (`get_node`, `find_by_prop`, `search_memories`,
`traverse`, `access_stats`, `search_vectors`); writes (`create_memory`, `create_entity`, `link`,
`set_node_props`, `remove_node`, `close_edge`, `set_embedding`, `submit_batch`); and
`get_changes`, the one unscoped read, which replays the op log across every scope and is therefore
off unless you pass `--allow-unscoped-changes`. Reads filter by a *set* of scopes (`--read-scopes`
at startup, or a per-call `scopes` array); a write is stamped with exactly *one* scope (`--scope`,
or a per-call `scope`) — `link` included, so an edge can join nodes living in different scopes.
Install with `cargo install topodb-mcp` and wire it into Claude Code or Claude Desktop in a couple
of lines. See [`crates/topodb-mcp/README.md`](crates/topodb-mcp/README.md) for the full CLI
reference, tool table, and client config examples.

- **Pi (pi.dev):** one command via `pi install npm:@topodb/pi` — see [topodb-mcp README → Pi](crates/topodb-mcp/README.md#pi).
