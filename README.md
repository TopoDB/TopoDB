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

Status: **early development (0.0.x), API not yet stable — pin exact
versions.** Shipping today: the engine (temporal property graph, scoped
k-hop traversal, BM25 + graph-scoped vector search, change feed, a
replay-deterministic op log), hybrid recall, and — over `topodb-mcp` — a
full memory-hygiene layer (write-time dedup and supersession,
contradiction-aware near-duplicate detection, and orphan / stale / health
maintenance scans), plus a Claude Code plugin that injects recall and a
hygiene nudge at session start. See [what's built](#whats-built).

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

# Store and link a fact in one call.
topodb --db agent.redb remember --content "ada wrote the first program" --entity ada
# → {"memory_id":"01…","deduplicated":false,"entities":[{"name":"ada","id":"01…","created":true}],…}
topodb --db agent.redb search "first program"
topodb --db agent.redb traverse 01… --max-hops 2
```

(The low-level `create-memory`, `create-entity`, and `link` commands are still available for
fine-grained control.)

To give a coding agent the same database as MCP tools:

```bash
cargo install topodb-mcp
claude mcp add topodb --transport stdio -- topodb-mcp --db /path/to/agent.redb
```

On [Pi](https://pi.dev) it is one command: `pi install npm:@topodb/pi`.

Inside Claude Code specifically, skip `cargo` and `claude mcp add` entirely —
install the plugin, which manages the server (including fetching it) for you:

```
/plugin marketplace add TopoDB/TopoDB
/plugin install topodb
```

See [`plugins/claude-code/README.md`](plugins/claude-code/README.md) for the
memory model and the risks it accepts (one database shared across every
project; the scope id is keyed to the absolute project path, so it does not
follow a repo across clones or machines).

To embed the engine directly in a Rust process, see the
[`topodb` crate example](crates/topodb/README.md) — the same graph, ops,
and scoped recall as a library call.

## What's built

Everything in the three groups below ships today (0.0.x — pin exact versions).

**Engine — `topodb`**

- Op-log write path — atomic batches, deterministic replay (property-tested)
- Single-applier concurrency; MVCC reads that never block each other or redb's storage commits (a long read can briefly delay the applier's next batch via registry guards)
- Scoped k-hop temporal traversal with `as_of` history reads
- Temporal edges — facts supersede, never overwrite
- Equality property index; BM25 full-text search (per-scope corpus)
- Graph-scoped vector search (cosine; embeddings host-computed, stored via `SetEmbedding`)
- Access stats (recall-driven counters); change feed (`subscribe` / `ops_since`) + op-log compaction
- Versioned on-disk format ([FORMAT.md](FORMAT.md))

**Memory & recall over MCP — `topodb-mcp`** (27 tools; [full table](crates/topodb-mcp/README.md))

- Hybrid recall — BM25 + vector + graph, RRF-fused, recency-weighted
- Memory hygiene — write-time dedup + supersession, banded/contradiction-aware near-duplicate detection, `consolidate_memories`, orphan + stale scans, `memory_health`, `suggest_links`
- Aliases and synonyms (`add_alias`, `add_synonym`) resolved into lookup and search
- Local embeddings (fastembed, on by default; ONNX Runtime auto-downloaded and sha256-pinned — Intel Macs still need a system runtime)
- Multi-scope reads — read across a scope *set*

**CLI & distribution — `topodb-cli`, `@topodb/pi`, plugin**

- All 17 engine operations, JSON in/out, exit-code contract (incl. `set-props` / `remove-node` / bulk submit)
- One-command Pi install (`@topodb/pi`); Claude Code plugin (managed server + session-start recall/hygiene injection)

**Planned:** multi-scope reads over the CLI · API stabilization (0.1) · reproducible benchmarks

**Never — by principle:** LLM calls inside the engine (principle 4) · a server process as a prerequisite (principle 5)

## Crates

| Crate | crates.io | What it is |
|---|---|---|
| [`topodb`](crates/topodb) | [![crates.io](https://img.shields.io/crates/v/topodb.svg)](https://crates.io/crates/topodb) | The embedded engine itself — link it into your process as a library. |
| [`topodb-json`](crates/topodb-json) | [![crates.io](https://img.shields.io/crates/v/topodb-json.svg)](https://crates.io/crates/topodb-json) | The shared JSON↔engine conversion layer used by `topodb-mcp` and `topodb-cli`. Not a library you typically depend on directly. |
| [`topodb-mcp`](crates/topodb-mcp) | [![crates.io](https://img.shields.io/crates/v/topodb-mcp.svg)](https://crates.io/crates/topodb-mcp) | An MCP (Model Context Protocol) server exposing a `topodb` database over stdio, for coding agents and other MCP clients that want scoped recall/write tools without embedding Rust. |
| [`topodb-cli`](crates/topodb-cli) | [![crates.io](https://img.shields.io/crates/v/topodb-cli.svg)](https://crates.io/crates/topodb-cli) | A direct-embedded `topodb` command-line binary — JSON in, JSON out, predictable exit codes — for scripting and ad hoc inspection of a database file without a server or an MCP client. |

### topodb-cli

`topodb-cli` installs a binary named **`topodb`**: point it at a `.redb` file and it gives you
all 17 engine operations (`info`, `create-memory`, `create-entity`, `link`, `get`, `find`,
`search`, `traverse`, `stats`, `changes`, `compact`, `set-props`, `remove-node`, `close-edge`,
`set-embedding`, `search-vector`, `submit`) as one-shot, script-friendly subcommands — compact
JSON on stdout, a `{"error":{"kind","message"}}` shape on stderr, and exit codes you can branch
on in a shell script (`0` success, `2` rejected/bad input, `1` internal/db-open failure).
`create-memory`, `create-entity`, and `link` each also take their own per-command `--scope`,
overriding the global `--scope` for that one invocation — the same override the batch DSL's
same-named ops and the equivalent `topodb-mcp` tools support. It opens the database file
directly and in-process, the same way `topodb-mcp` does — no server, no network hop, and
(because of that) no running concurrently with something else that already has the same file
open. Install with `cargo install topodb-cli`. See
[`crates/topodb-cli/README.md`](crates/topodb-cli/README.md) for the full command table,
exit-code contract, scoping rules, and v1 limitations (no `--spec` flag; no multi-scope reads —
this CLI reads under one scope at a time, while `topodb-mcp` can read across a set; direct-embedded
single-process access only).

### topodb-mcp

A standalone binary: point it at a `.redb` file and it serves **27 MCP tools** over stdio
JSON-RPC. In brief (the [full tool table](crates/topodb-mcp/README.md) lives in the crate README):

- **Recall & read** — `search_memories` (hybrid BM25 + vector + graph, RRF-fused), `recent_memories`, `traverse`, `suggest_links`, `get_node`, `find_by_prop`, `get_edges`, `access_stats`, `search_vectors`
- **Memory hygiene** — `find_duplicate_memories` (vector-mode: banded + contradiction-aware; text-mode fallback when embedder unavailable), `find_orphan_memories`, `find_stale_memories`, `memory_health`
- **Write** — `remember`, `create_memory`, `consolidate_memories`, `create_entity`, `add_alias`, `add_synonym`, `link`, `set_node_props`, `remove_node`, `close_edge`, `set_embedding`, `submit_batch`
- **Admin** — `db_info`; `get_changes` (the one unscoped read — replays the op log across every scope, so it's off unless you pass `--allow-unscoped-changes`)

**Scoping.** Reads filter by a *set* of scopes (`--read-scopes`, or a per-call `scopes` array);
a write is stamped with exactly *one* scope (`--scope`, or a per-call `scope`) — `link` included,
so an edge can join nodes in different scopes.

**Embeddings.** `--embeddings` is on by default and auto-fetches an ONNX Runtime on first run
(system runtimes and `ORT_DYLIB_PATH` win; `--no-ort-download` disables it). Intel Macs have no
official 1.24.2 artifact and use the manual path; with no runtime the server runs text+graph-only.

Install with `cargo install topodb-mcp` (or on **Pi**: `pi install npm:@topodb/pi`). See
[`crates/topodb-mcp/README.md`](crates/topodb-mcp/README.md) for the full CLI reference, tool
table, and client config.
