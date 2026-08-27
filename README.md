# TopoDB

[![crates.io](https://img.shields.io/crates/v/topodb.svg)](https://crates.io/crates/topodb)
[![docs.rs](https://img.shields.io/docsrs/topodb)](https://docs.rs/topodb)

TopoDB is an embedded memory engine for AI agents, written in Rust.
It runs in-process, and there is no server.
Facts are stored on a temporal property graph and supersede rather than
overwrite.
Recall is scoped, and search uses BM25 and graph-scoped vectors.
A change feed supports work outside the engine.

Status is 0.1, with breaking changes reserved for 0.2.0; however, the
on-disk format migrates in place.

![Scope view of a small TopoDB graph](assets/graph.png)

`topodb graph --graph-format html --out graph.html` writes a self-contained
viewer.

## A session

The usual path is a plugin, not a server.
Install it in Cursor or Claude Code, then open a project.
The next chat receives up to eight recent memories for that project.
A one-line hygiene notice appears when the store has duplicates,
contradictions, orphans, or cold memories.

The agent is expected to search before it asks you to repeat a fact.
Durable decisions are stored with `remember`.
At the end of a session that produced something worth keeping and wrote
nothing, a nudge asks it to remember.

There is no hosted service.
The engine runs in-process beside the editor.
Policy — what to keep, what to ignore, when to merge — is not inside the
database.
It lives in `CONVENTIONS.md`, written next to the database on first boot,
and in the `topodb-memory` skill.

## What an agent should remember

Store decisions and the reasoning behind them, constraints, ownership, and
lessons that will still be true tomorrow.
Do not store what git and the code already record, or anything that only
matters in this conversation.

When two memories are the same fact in different words, merge them with
`consolidate_memories`.
The agent picks which copy to keep.
The engine will not infer a merge from similarity, because contradictions
score high too.

When a fact is replaced, pass `supersedes` on `remember`.
When a memory should never surface again, `forget` it.
`lifecycle_candidates` ranks cold memories and proposes; the agent acts.

`memory_health` reports duplicate pairs, supersessions, orphans, and stale
rows.
Those scans never delete.

When `remember` returns `supersession_candidates`, the agent supersedes the
stale side, consolidates a duplicate, or ignores a false alarm.
Uncertainty stays in the graph until something is judged.

The same rules are in
[`crates/topodb-onboarding/templates/CONVENTIONS.md`](crates/topodb-onboarding/templates/CONVENTIONS.md)
and [`plugins/cursor/skills/topodb-memory/SKILL.md`](plugins/cursor/skills/topodb-memory/SKILL.md).

## Principles

1. Narrow and deep — one workload done excellently
2. Format stability is a feature — versioned on-disk format, migrations always
3. Honest benchmarks from day one
4. Engine, not policy — no LLM calls inside the database, ever
5. Embedded-first — servers and sync are future layers, never prerequisites

## Install

For Cursor or Claude Code, install the plugin first.
The CLI below is the same engine without an editor.

```bash
cargo install topodb-cli          # binary name: topodb
topodb --db agent.redb remember --content "ada wrote the first program" --entity ada
topodb --db agent.redb search "first program"
```

The CLI opens the file in-process. A second process holding the same file
will fail.

| Client | Install |
|---|---|
| Claude Code | `/plugin marketplace add TopoDB/TopoDB` then `/plugin install topodb` |
| Cursor | Import `https://github.com/TopoDB/TopoDB` (`plugins/cursor`) |
| Pi | `pi install npm:@topodb/pi` |
| Any MCP client | `cargo install topodb-mcp` — [`docs/agent-clients.md`](docs/agent-clients.md) |
| Rust | [`crates/topodb`](crates/topodb/README.md) |
| Node.js (embedded) | `npm i topodb` — [`crates/topodb-node`](crates/topodb-node/README.md) |
| Python (embedded) | `pip install topodb` — [`crates/topodb-py`](crates/topodb-py/README.md) |

Scope ids are keyed to the absolute project path.
They do not follow a repo across clones or machines.
[`plugins/claude-code/README.md`](plugins/claude-code/README.md) ·
[`crates/topodb-cli`](crates/topodb-cli/README.md) ·
[`crates/topodb-mcp`](crates/topodb-mcp/README.md) ·
[FORMAT.md](FORMAT.md) ·
[topodb.dev](https://topodb.dev)

## Benchmarks

[LongMemEval-S](https://github.com/xiaowu0162/LongMemEval), 500 questions.
The embedder is held constant, so the figure is ranking.
Hybrid Recall@5 is **0.987** at turn granularity.

| Leg | R@1 | R@3 | R@5 | R@10 |
|-----|-----|-----|-----|------|
| text (BM25)     | 0.872 | 0.932 | 0.953 | 0.979 |
| vector          | 0.864 | 0.953 | 0.977 | 0.989 |
| hybrid (RRF)    | 0.894 | 0.966 | 0.987 | 0.996 |

Per-turn memories lift hybrid R@1 from 0.832 to **0.894**.
The co_mention graph leg is neutral here.
[`benchmarks/longmemeval/RESULTS.md`](benchmarks/longmemeval/RESULTS.md)

## Crates

| Crate | crates.io | What it is |
|---|---|---|
| [`topodb`](crates/topodb) | [![crates.io](https://img.shields.io/crates/v/topodb.svg)](https://crates.io/crates/topodb) | The engine |
| [`topodb-cli`](crates/topodb-cli) | [![crates.io](https://img.shields.io/crates/v/topodb-cli.svg)](https://crates.io/crates/topodb-cli) | `topodb` binary |
| [`topodb-mcp`](crates/topodb-mcp) | [![crates.io](https://img.shields.io/crates/v/topodb-mcp.svg)](https://crates.io/crates/topodb-mcp) | MCP server |
| [`topodb-json`](crates/topodb-json) | [![crates.io](https://img.shields.io/crates/v/topodb-json.svg)](https://crates.io/crates/topodb-json) | JSON↔engine layer |
| [`topodb-obsidian`](crates/topodb-obsidian) | [![crates.io](https://img.shields.io/crates/v/topodb-obsidian.svg)](https://crates.io/crates/topodb-obsidian) | Vault ingest/seed |
| [`topodb-warehouse`](crates/topodb-warehouse) | — | Session-artifact log |
| [`topodb-sgh`](crates/topodb-sgh) | [![crates.io](https://img.shields.io/crates/v/topodb-sgh.svg)](https://crates.io/crates/topodb-sgh) | Optional agent harness |

License: MIT OR Apache-2.0.
