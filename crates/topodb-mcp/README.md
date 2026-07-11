# topodb-mcp

An MCP (Model Context Protocol) server exposing the [TopoDB](https://crates.io/crates/topodb)
agent-memory engine over stdio. Point an MCP client at a `.redb` database file and it gets
recall (get/find/search/traverse/access-stats/changes) and write (create memory, create
entity, link) tools backed by a scoped, temporal property graph — no separate database
process, no network hop.

Status: **v0** — read + write tools, no vector search yet. See [Limitations](#v0-limitations).

## Install

```bash
cargo install topodb-mcp
```

This installs the `topodb-mcp` binary to your Cargo bin directory (typically `~/.cargo/bin`),
which must be on `PATH` for the client configs below to find it by name.

## CLI reference

```
topodb-mcp --db <path> [--scope <ulid|shared>] [--spec <path>]
```

| Flag | Required | Default | Meaning |
|---|---|---|---|
| `--db <path>` | yes | — | Path to the redb database file. A missing *file* is created on open; a missing *parent directory* is a startup error. |
| `--scope <ulid\|shared>` | no | `shared` | The default scope applied to every tool call that omits its own `scope` parameter. `"shared"` (case-insensitive) resolves to the shared scope; any other value is parsed as a ULID and resolves to that scope id. An invalid value is a startup error. |
| `--spec <path>` | no | inherit / built-in default | Path to a JSON file deserializing to `topodb::IndexSpec`, controlling which `(label, prop)` pairs are equality- or text-indexed, honored verbatim (may reindex an existing db). Omitted: an **existing** db inherits its own persisted spec (never reindexed or clobbered); a **fresh** db is created with the built-in default — equality index on `(Entity, name)`, text index on `(Memory, content)`, matching the labels/props the `create_entity`/`create_memory` tools write, so lookup and search work out of the box with no spec file. This mirrors `topodb-cli`, so a db either tool created is served identically by the other. |

Arg parsing is hand-rolled (three flags); there is no `--help` flag yet.

## Tools

`tools/list` reports exactly 10 tools: `db_info`, six read tools, three write tools.

| Tool | Params | Description |
|---|---|---|
| `db_info` | — | Report the open database's path, current op-log sequence number, and the default scope applied to tool calls that omit `scope`. Call this first to confirm the server is wired to the expected database, and to obtain `current_seq` as the anchor for `get_changes`. |
| `get_node` | `id` (string, required); `scope` (string, optional) | Fetch one node by its ULID. Call this when you already have a node id (from a previous search, traverse, or create) and need its current label and properties. |
| `find_by_prop` | `label`, `prop`, `value` (string/number/bool), `scope?` | Exact-match lookup on an equality-indexed property (e.g. an Entity's name). Call this to resolve a known identifier to a node — NOT for fuzzy or full-text search; use `search_memories` for that. Errors if `(label, prop)` is not declared in the index spec. |
| `search_memories` | `query` (required), `k` (integer, default 10), `scope?` | Full-text BM25 search over indexed text properties. Call this when looking for memories relevant to a topic or phrase. Returns up to `k` nodes ranked by relevance with scores. |
| `traverse` | `seed_id` (required), `max_hops` (integer, default 2), `direction` (enum: `out`/`in`/`both`, default `both`), `edge_types` (array of strings, optional), `scope?` | Walk the graph outward from a seed node, following edges up to `max_hops`. Call this to gather the context AROUND something you already found — related entities, linked memories. Returns the subgraph (nodes + edges). |
| `access_stats` | `id` (required), `scope?` | Read a node's access statistics (count, last-accessed timestamp). Call this when deciding what to consolidate or forget — e.g. finding stale memories. Reading stats does not itself count as an access. |
| `get_changes` | `since_seq` (integer, required) | Replay the operation log from a sequence number (inclusive). Host-level primitive for consolidation/sync — the ONE unscoped read; the log spans all scopes. Returns ops with their seq numbers; on Compacted errors, re-anchor from current state. The `db_info` tool reports `current_seq`. |
| `create_memory` | `content` (string, required), `props` (object, optional), `scope?` | Store a new memory. Call this when the user or task produces information worth remembering later. `content` becomes the full-text-searchable body; `props` holds structured metadata (strings/numbers/bools). Returns the new node's id — keep it if you plan to link this memory to entities. |
| `create_entity` | `name` (string, required), `props` (object, optional), `scope?` | Create an entity node (person, project, concept). Call this the FIRST time something is mentioned that memories should attach to; use `find_by_prop` first to check it doesn't already exist. `name` is equality-indexed for exact lookup. |
| `link` | `from_id`, `to_id`, `edge_type` (all required strings), `props` (object, optional), `valid_from` (integer ms, optional) | Create a typed, time-aware edge between two existing nodes. Call this to connect a memory to the entities it concerns, or entities to each other (e.g. `'works_on'`). `edge_type` is free-form but be consistent — `traverse` can filter by it. Returns the edge id. Errors if either node doesn't exist. |

Every scoped tool that omits `scope` uses the server's configured `--scope` default (see
[Scoping semantics](#scoping-semantics)). Engine errors and parse failures are returned as MCP
tool errors carrying the engine's message — the server never panics on bad input.

## Client configuration

### Claude Code

Register a local stdio server with [`claude mcp add`](https://code.claude.com/docs/en/mcp).
Everything after the `--` separator is passed to `topodb-mcp` untouched:

```bash
claude mcp add topodb --transport stdio -- topodb-mcp --db /path/to/agent.redb
```

> **Watch the `--scope` collision.** `claude mcp add` has its own `-s`/`--scope` flag
> (registration scope: `local`/`project`/`user` — where the server config is *stored*). That is
> unrelated to `topodb-mcp`'s own `--scope` flag (the default *recall* scope inside the
> database). Because ours comes after `--`, it's passed straight to the binary and there's no
> actual collision — but if you also want to set Claude Code's registration scope, put its
> `--scope`/`-s` *before* the `--`:
>
> ```bash
> claude mcp add topodb --transport stdio --scope user -- topodb-mcp --db /path/to/agent.redb --scope shared
> ```

If you'd rather edit config directly, the equivalent stdio entry (project `.mcp.json` or
`~/.claude.json`) is:

```json
{
  "mcpServers": {
    "topodb": {
      "command": "topodb-mcp",
      "args": ["--db", "/path/to/agent.redb"]
    }
  }
}
```

### Claude Desktop

Add an entry to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "topodb": {
      "command": "topodb-mcp",
      "args": ["--db", "/path/to/agent.redb"]
    }
  }
}
```

Use an absolute path for `--db`. Claude Desktop spawns the server with its own environment,
which may not include the same `PATH` as your shell — if `topodb-mcp` isn't found, replace
`"command": "topodb-mcp"` with the absolute path to the installed binary
(e.g. `~/.cargo/bin/topodb-mcp` on macOS/Linux, `%USERPROFILE%\.cargo\bin\topodb-mcp.exe` on
Windows).

### Pi

**One command** — the [`@topodb/pi`](https://www.npmjs.com/package/@topodb/pi)
extension bundles everything (no Rust, no separate MCP adapter):

    pi install npm:@topodb/pi

It registers a `topodb` tool that spawns this server for you. Config via env:
`TOPODB_DB` (default `.topodb/memory.redb`), `TOPODB_SCOPE` (default `shared`).

**Manual (any MCP server on Pi)** — Pi has no built-in MCP client, so install an
MCP client extension once, then point it at `topodb-mcp`:

    pi install npm:pi-mcp-adapter

Then add topodb to the config that adapter reads (`~/.pi/agent/mcp.json` global,
or `.mcp.json` project):

```json
{
  "mcpServers": {
    "topodb": {
      "command": "npx",
      "args": ["-y", "@topodb/topodb-mcp", "--db", ".topodb/memory.redb"],
      "lifecycle": "lazy"
    }
  }
}
```

(`pi-mcp-extension` works too, but reads `.pi/mcp.json` instead of `.mcp.json`.)

## Scoping semantics

TopoDB partitions nodes and edges into scopes — a shared scope plus any number of ULID-named
scopes — so multiple agents or conversations can share one database file without stepping on
each other's memories. `topodb-mcp` resolves scope as follows:

- The server is started with one **default scope** (`--scope`, default `shared`).
- Every scoped tool call (all of them except `get_changes`) accepts an optional `scope`
  parameter. Omit it to use the server's default scope; pass `"shared"` or a scope ULID to
  target a specific scope explicitly.
- `link` has no `scope` parameter on the wire — it always writes to the server's configured
  default scope.
- `get_changes` is the one deliberately **unscoped** tool: the operation log spans every scope,
  so a host can replay it for cross-scope consolidation or sync. There is no way to filter it
  by scope.

If you want per-conversation isolation, start a separate `topodb-mcp` process per conversation
with a distinct `--scope <ulid>` against the same `--db` file (or pass `scope` explicitly on
each tool call from a single server instance).

## v0 limitations

- **No vector search.** MCP clients don't carry embedders, and accepting raw vector params over
  the wire invites garbage queries — this is deferred to a future version with a real embedding
  story. `search_memories` is BM25 full-text only.
- **No `set_props` / `remove_node`.** The write surface is create-only (`create_memory`,
  `create_entity`, `link`); mutating or deleting existing nodes isn't exposed yet.
  Corrections must go through a fresh fact (TopoDB facts supersede, they don't overwrite).
- **`Bytes` and `DateTime` prop values are unsupported over MCP.** Only string, integer, float,
  and bool prop values round-trip through JSON; attempting to write or a stored node that
  contains a `Bytes`/`DateTime` prop is rejected/errors rather than silently coerced.
- **`link` always uses the default scope.** There is no way to create a cross-scope edge or an
  edge in a non-default scope through this tool in v0 — start a server with the target scope as
  its default, or wait for a future `scope` param on `link`.
- **No HTTP/SSE transport.** Only stdio, i.e. one client process per server process.
