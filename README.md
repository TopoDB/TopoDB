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

Status: **early development (0.0.x)** — the engine core **and the recall
layer** are implemented: op-log write path, single-applier concurrency,
scoped k-hop temporal traversal, BM25 full-text search, graph-scoped vector
search, access stats, change feed, and replay-determinism property tests.
API not yet stable — pin exact versions. See
[implemented vs planned](#implemented-vs-planned).

First consumer: Atlas (agentic OS desktop app).

## Principles

1. Narrow and deep — one workload done excellently
2. Format stability is a feature — versioned on-disk format, migrations always
3. Honest benchmarks from day one
4. Engine, not policy — no LLM calls inside the database, ever
5. Embedded-first — servers and sync are future layers, never prerequisites

Principle 4 is a hard boundary, not a preference: anything LLM-driven —
summarization, reflection, consolidation — is a **layer built on the
engine**, never a feature inside it. The engine's job is to hand those
layers the primitives they need: the change feed, temporal history, and
scoped recall.

## Five-minute quick start

The fastest path is the CLI (installs a binary named `topodb`):

```bash
cargo install topodb-cli

# Create a database, an entity, and a memory — then link and search them.
topodb --db agent.redb create-entity --name ada
# → {"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}
topodb --db agent.redb create-memory --content "ada wrote the first program"
# → {"id":"01BX5ZZKBKACTAV9WEVGEMMVRZ"}
topodb --db agent.redb link --from 01BX5ZZKBKACTAV9WEVGEMMVRZ --to 01ARZ3NDEKTSV4RRFFQ69G5FAV --type ABOUT
topodb --db agent.redb search "first program"
topodb --db agent.redb traverse 01BX5ZZKBKACTAV9WEVGEMMVRZ --max-hops 2
```

(Substitute the ids your own `create-*` calls print.)

To give a coding agent the same database as MCP tools:

```bash
cargo install topodb-mcp
claude mcp add topodb --transport stdio -- topodb-mcp --db /path/to/agent.redb
```

On [Pi](https://pi.dev) it is one command: `pi install npm:@topodb/pi`.

To embed the engine directly in a Rust process, see the
[`topodb` crate example](crates/topodb/README.md) — the same graph, ops,
and scoped recall as a library call.

## Implemented vs planned

| Capability | Where | Status |
|---|---|---|
| Op-log write path — atomic batches, deterministic replay (property-tested) | engine | ✅ |
| Single-applier concurrency; lock-free snapshot reads | engine | ✅ |
| Scoped k-hop temporal traversal (`as_of` history reads) | engine | ✅ |
| Temporal edges — facts supersede, never overwrite | engine | ✅ |
| Equality property index | engine | ✅ |
| BM25 full-text search (per-scope corpus) | engine | ✅ |
| Graph-scoped vector search (cosine; embeddings host-computed, stored via `SetEmbedding`) | engine | ✅ |
| Access stats (recall-driven counters) | engine | ✅ |
| Change feed (`subscribe` / `ops_since`) + op-log compaction | engine | ✅ |
| Versioned on-disk format ([FORMAT.md](FORMAT.md)) | engine | ✅ |
| MCP server (10 tools) | `topodb-mcp` | ✅ v0 |
| CLI (all 11 engine operations) | `topodb-cli` | ✅ v1 |
| One-command Pi install | `@topodb/pi` | ✅ |
| Vector search exposed over MCP / CLI | layers | Planned |
| `set-props` / `remove-node` / bulk submit over CLI | `topodb-cli` | Planned |
| API stabilization (0.1) | engine | Planned |
| Reproducible benchmarks | repo | Planned |
| LLM calls inside the engine | — | **Never** (principle 4) |
| Server process as a prerequisite | — | **Never** (principle 5) |

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

`topodb-mcp` is a standalone binary: point it at a `.redb` file and it serves 10 MCP tools —
`db_info`, six read tools (`get_node`, `find_by_prop`, `search_memories`, `traverse`,
`access_stats`, `get_changes`), and three write tools (`create_memory`, `create_entity`,
`link`) — over stdio JSON-RPC. Install with `cargo install topodb-mcp` and wire it into Claude
Code or Claude Desktop in a couple of lines. See
[`crates/topodb-mcp/README.md`](crates/topodb-mcp/README.md) for the full CLI reference, tool
table, client config examples, and v0 limitations (no vector search yet, create-only writes).

- **Pi (pi.dev):** one command via `pi install npm:@topodb/pi` — see [topodb-mcp README → Pi](crates/topodb-mcp/README.md#pi).
