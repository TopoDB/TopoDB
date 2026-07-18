# topodb-mcp

An MCP (Model Context Protocol) server exposing the [TopoDB](https://crates.io/crates/topodb)
agent-memory engine over stdio. Point an MCP client at a `.redb` database file and it gets
recall (get/find/search-memories/search-vectors/traverse/access-stats/changes) — hybrid
BM25 + vector + graph search under the hood — and write (create memory, create entity, add
alias, add synonym, link, set-props, remove-node, close-edge, set-embedding, batch) tools
backed by a scoped, temporal property graph — no separate database process, no network hop.

Status: **v0** — read + write tools, including vector search and node/edge mutation.
See [Limitations](#v0-limitations).

## Install

```bash
cargo install topodb-mcp
```

This installs the `topodb-mcp` binary to your Cargo bin directory (typically `~/.cargo/bin`),
which must be on `PATH` for the client configs below to find it by name.

## CLI reference

```
topodb-mcp --db <path> [--scope <ulid|shared>] [--read-scopes <ulid|shared>[,...]]
           [--spec <path>] [--allow-unscoped-changes]
           [--embeddings <off|auto|model>] [--model-dir <path>]
```

| Flag | Required | Default | Meaning |
|---|---|---|---|
| `--db <path>` | yes | — | Path to the redb database file. A missing *file* is created on open; a missing *parent directory* is a startup error. |
| `--scope <ulid\|shared>` | no | `shared` | The default **write** scope: the one scope a create/link tool call is stamped with when it omits its own `scope` parameter. `"shared"` (case-insensitive) resolves to the shared scope; any other value is parsed as a ULID and resolves to that scope id. An invalid value is a startup error. |
| `--read-scopes <list>` | no | `--scope`'s value | The default **read** scope *set*: a comma-separated list of `"shared"`/scope ULIDs (whitespace around entries ignored) that a read tool call filters by when it omits both its own `scope` and `scopes` parameters. Reads filter by a set; `--scope` picks the single scope a write is stamped with — that asymmetry is why there are two flags instead of one. An empty list is a startup error (there is no unscoped read). |
| `--spec <path>` | no | inherit / built-in default | Path to a JSON file deserializing to `topodb::IndexSpec`, controlling which `(label, prop)` pairs are equality- or text-indexed, honored verbatim (may reindex an existing db). Omitted: an **existing** db inherits its own persisted spec — except that a db still on an older *stock* default (never `--spec`-customized) is silently upgraded to the current default with a one-time reindex (e.g. picking up the `(Entity, name)`/`(Alias, name)`/`(Synonym, term)` indexes); customized specs are never rewritten. A **fresh** db is created with the built-in default — equality index on `(Entity, name)`, `(Alias, name)`, and `(Synonym, term)`, text index on `(Memory, content)`, `(Entity, name)`, and `(Alias, name)` — matching the labels/props the write tools produce, so lookup and search (including entity names, aliases, and synonyms) work out of the box with no spec file. This mirrors `topodb-cli`, so a db either tool created is served identically by the other. |
| `--allow-unscoped-changes` | no | off | Bare toggle. `get_changes` is the one deliberately unscoped read tool — its op log spans every scope in the db — so it is rejected with `invalid_params` unless the server was started with this flag. Sync/consolidation hosts that legitimately need the whole log pass it. |
| `--embeddings <off\|auto\|model>` | no | auto (`bge-small-en-v1.5`, 384-dim) | Controls the local embedding subsystem used for the vector leg of `search_memories` and for `set_embedding`-free automatic write-path embedding. `off` disables it outright (permanently `EmbedderStatus::Off` — text+graph-only recall, no download, no model load). Omitted, or `auto` (case-insensitive — an explicit spelling of the same default), starts the default model. Any other value names a different model to load (unrecognized names still start the embedder but land in `failed` status rather than refusing to start). The chosen model's weights are downloaded once (~34MB for the default) into the model-dir cache and reused after that; `db_info`'s `embeddings.status` reports `off`/`downloading`/`ready`/`failed` so a client can tell when semantic recall becomes available. **Requires an ONNX Runtime dynamic library on the host** — this server is built with fastembed's `ort-load-dynamic`, so embeddings reach `ready` only once a compatible ONNX Runtime dylib is discoverable (e.g. `brew install onnxruntime`, or point `ORT_DYLIB_PATH` at one directly, e.g. `/usr/local/lib/libonnxruntime.dylib`). Without it, status is `failed` and the server runs normally in text+graph-only mode — nothing else degrades. |
| `--model-dir <path>` | no | `~/.cache/topodb/models` (falls back to `./.topodb-models` if `$HOME` is unset) | Directory the embedding model's weights are downloaded into and loaded from. Shared across servers/projects when left at its default, so the one-time download only happens once per machine. |

Arg parsing is hand-rolled (seven flags); there is no `--help` flag yet.

## Tools

`tools/list` reports exactly 19 tools: `db_info`, eight read tools (`get_changes` included), and
ten write tools.

| Tool | Params | Description |
|---|---|---|
| `db_info` | — | Report the open database's path, current op-log sequence number, the default WRITE scope applied to a create/link call that omits `scope`, the default READ scope set applied to a read call that omits both `scope`/`scopes`, and an `embeddings: { model, status }` field (`status`: `off`/`downloading`/`ready`/`failed`) reporting the local embedding subsystem's state. Call this first to confirm the server is wired to the expected database and read set, to obtain `current_seq` as the anchor for `get_changes`, and to know whether `search_memories` currently has a vector leg available. |
| `get_node` | `id` (string, required); `scope` (string, optional); `scopes` (string[], optional) | Fetch one node by its ULID. Call this when you already have a node id (from a previous search, traverse, or create) and need its current label and properties. |
| `find_by_prop` | `label`, `prop`, `value` (string/number/bool), `exact` (bool, default false), `scope?`, `scopes?` | Lookup on an equality-indexed property (e.g. an Entity's name). String values match case- and whitespace-insensitively by default; `exact: true` requires a byte-exact match. For `(Entity, name)` with `exact: false`, also resolves through registered aliases (`add_alias`) to their canonical entity. Call this to resolve a known identifier to a node — for topic/phrase search use `search_memories`. Errors if `(label, prop)` is not declared in the index spec. |
| `search_memories` | `query` (required), `k` (integer, default 10), `recency_weight` (0-1, default 0.3), `recency_half_life_days` (default 30), `fuzzy` (bool, default true), `graph_boost` (bool, default true), `scope?`, `scopes?` | Hybrid recall (`Db::recall`) fusing up to three legs with Reciprocal Rank Fusion (k=60): a BM25 **text** leg over indexed content and entity names (camelCase-split, Snowball-stemmed, with miss-only fuzzy/prefix fallback at a 0.6× discount unless `fuzzy: false`, and learned synonyms — `add_synonym` — expanded into the query automatically); a cosine **vector** leg using an automatically-computed embedding of `query` whenever the server's embedder status is `ready` (silently omitted otherwise — see `db_info`); and, when `graph_boost` is true (default), a **graph** leg that takes the top 5 preliminary text+vector hits as seeds, pulls in their 1-hop neighbors at half weight, and folds them into the same fusion. Recency weighting is applied once, after fusion: each hit's fused score is multiplied by `(1-w) + w·2^(-age/half_life)`, so fresher memories outrank stale ones at equal relevance (`recency_weight: 0` restores pure fused ranking). Returns up to `k` nodes ranked with scores. |
| `get_edges` | `from_id` (required), `to_id?`, `edge_type?`, `open_only` (bool, default true), `scope?`, `scopes?` | List a node's outgoing edges, optionally filtered by target node and/or edge type; open edges only by default. This is how a client finds the edge id to `close_edge` when a fact stops being true, and how it checks what a node is already linked to. An `edge_type` filter matches both the normalized and raw stored forms. |
| `traverse` | `seed_id` (required), `max_hops` (integer, default 2), `direction` (enum: `out`/`in`/`both`, default `both`), `edge_types` (array of strings, optional), `scope?`, `scopes?` | Walk the graph outward from a seed node, following edges up to `max_hops`. Call this to gather the context AROUND something you already found — related entities, linked memories. Returns the subgraph (nodes + edges). |
| `access_stats` | `id` (required), `scope?`, `scopes?` | Read a node's access statistics (count, last-accessed timestamp). Call this when deciding what to consolidate or forget — e.g. finding stale memories. Reading stats does not itself count as an access. |
| `search_vectors` | `model` (string, required), `vector` (number array, required), `k` (integer, default 10), `candidates` (array of node ids, optional), `scope?`, `scopes?` | Cosine similarity search over embeddings stored under `model`. Call this when you have a host-computed query embedding and want nodes ranked by vector similarity rather than text relevance. `candidates` restricts scoring to a given node id set (e.g. narrow to a `traverse` result for hybrid recall). Errors if `k` is 0 or the vector is empty. |
| `get_changes` | `since_seq` (integer, required) | Replay the operation log from a sequence number (inclusive). Host-level primitive for consolidation/sync — the ONE unscoped read; the log spans all scopes. Returns ops with their seq numbers; on Compacted errors, re-anchor from current state. The `db_info` tool reports `current_seq`. Rejected with `invalid_params` unless the server was started with `--allow-unscoped-changes`. |
| `create_memory` | `content` (string, required), `props` (object, optional), `scope?` | Store a new memory. Call this when the user or task produces information worth remembering later. `content` becomes the full-text-searchable body; `props` holds structured metadata (strings/numbers/bools). Returns the new node's id — then link it to the entities it concerns. |
| `create_entity` | `name` (string, required), `props` (object, optional), `scope?` | **Find-or-create** an entity node (person, project, concept). The name is matched case- and whitespace-insensitively across the read scopes, the write scope, and `shared`, and — via registered aliases — resolves an alternate name to its canonical entity too; an existing entity is returned with `created: false` (oldest node wins when pre-existing duplicates match) and any NEW `props` keys are merged without overwriting. Only when nothing matches is a node created (`created: true`). |
| `add_alias` | `entity_id` (string, required), `alias` (string, required), `scope?` (defaults to the entity's own scope) | Register an alternate name for an existing entity ("Drew" for "Drew Powell", "the broker" for "launch.js"). From then on `create_entity`, `find_by_prop`, and search resolve the alias to the canonical entity — use this the moment you learn a second name for something instead of creating a duplicate. Errors if the alias already names a DIFFERENT entity (that's a merge situation; both ids are reported). Idempotent for the same entity. Remove an alias with `remove_node` on the alias node id. |
| `add_synonym` | `term` (string, required), `expansion` (string, required), `bidirectional` (bool, default true), `scope?` (defaults to the server's write scope) | Teach search a domain equivalence: after `add_synonym('auth','login')`, searching "auth" also matches memories that say "login" (at a discount, so exact matches still win). Bidirectional by default. Use when you learn this project's vocabulary — "broker" meaning `launch.js`, "the engine" meaning `crates/topodb`. Depth-1 only: synonyms never chain. Remove with `remove_node` on the synonym node id. |
| `link` | `from_id`, `to_id`, `edge_type` (all required strings), `supersede` (bool, default false), `props` (object, optional), `valid_from` (integer ms, optional), `scope?` | Create (or reuse) a typed, time-aware edge. `edge_type` is normalized (lowercased; whitespace/hyphens collapse to `_`, so `Works At` == `works_at`). Idempotent per `(from, to, type)` within the write scope: an identical open edge is returned with `created: false` instead of a duplicate. `supersede: true` atomically closes every other open same-type edge from `from` (the to-one-relation-changed flow) and reports them in `superseded`. `valid_from` must be a plausible past-or-present ms timestamp (seconds-since-epoch and future values are rejected). Errors if either node doesn't exist. |
| `set_node_props` | `id` (string, required), `props` (object, required — a `null` value REMOVES that key) | Set or remove properties on an existing node. Errors if the node doesn't exist. Returns the committed seq. |
| `remove_node` | `id` (string, required) | Hard-delete a node and cascade-remove its incident edges. Call this to forget something entirely. Errors if the node doesn't exist. Returns the committed seq. |
| `close_edge` | `id` (string, required), `valid_to` (integer ms, optional — defaults to now) | Close an open edge, stamping its `valid_to` — the fact stops being "currently true" but stays in history. Find the edge id with `get_edges`; for the "X changed to Y" case prefer `link` with `supersede: true`. An explicit `valid_to` must be a plausible past-or-present ms timestamp (seconds-since-epoch and future values are rejected). Errors if the edge doesn't exist or is already closed. Returns the committed seq. |
| `set_embedding` | `id` (string, required), `model` (string, required), `vector` (non-empty number array, required) | Attach a raw embedding vector (host-computed) to an existing node under `model`. Errors if the node doesn't exist, the vector is empty, or its dimension conflicts with the model's existing vectors. Returns the committed seq. |
| `submit_batch` | `commands` (array of command objects, required) | Submit a batch of high-level commands atomically — all commit or none. Each command's `op` matches a tool name (own field names, not always identical to that tool's param names — see the batch DSL). `#N` in an id field references the id produced by the Nth earlier command. Returns the produced ids in order (`null` for commands that create nothing). |

Every scoped read tool accepts both `scope` (one scope) and `scopes` (an array of several,
e.g. a project scope plus `"shared"`) — a non-empty `scopes` wins over `scope`, which wins over
the server's configured default read set (`--read-scopes`, or `--scope` alone). An explicitly
empty `scopes: []` is rejected (`invalid_params`) rather than treated as "read everything" —
there is no unscoped read except `get_changes`, gated separately behind
`--allow-unscoped-changes`. Every write tool accepts only `scope` (one scope) — see
[Scoping semantics](#scoping-semantics) for the full reads-filter-a-set-writes-stamp-one
picture. `scopes` is not a write-tool param at all: every param struct rejects an unknown
field rather than silently ignoring it, so passing `scopes` to a write tool is a clean tool
error, not a quiet no-op. Engine errors and parse failures are returned as MCP tool errors
carrying the engine's message — the server never panics on bad input.

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
each other's memories. **Reads filter by a *set* of scopes; a write is stamped with exactly
*one*.** That asymmetry is the thing to get right: a read tool can gather results from several
scopes at once (e.g. a private project scope plus `shared`), but a create/link tool call always
picks exactly one scope for the node/edge it produces. `topodb-mcp` resolves scope as follows:

- The server is started with two independent defaults:
  - `--scope` (default `shared`) — the default **write** scope, used by `create_memory`,
    `create_entity`, and `link` when a call omits its own `scope`.
  - `--read-scopes` (default: `--scope`'s value alone) — the default **read** scope *set*, used
    by every scoped read tool when a call omits both `scope` and `scopes`. Comma-separated; an
    empty list is a startup error — there is no unscoped read.
- Every scoped read tool call also accepts, per call: an optional `scope` (one scope), and an
  optional `scopes` (an array of several). Precedence: a non-empty `scopes` wins over `scope`,
  which wins over the server's default read set. An explicitly empty `scopes: []` is **rejected**
  (`invalid_params`) rather than treated as "read everything".
- Every write tool call accepts only a single optional `scope`, resolved against the server's
  default *write* scope (`--scope`), never against `--read-scopes`. `link` is the exception worth
  noting: its `scope` param determines which scope the *edge itself* lives in, independent of the
  scopes of the two nodes it connects — this is what lets an edge join nodes that live in a scope
  other than the server's default, e.g. an edge from a `shared`-scope entity to a private-scope
  memory. The batch DSL's `link` op takes the same `scope` field.
- `get_changes` is the one deliberately **unscoped** tool: the operation log spans every scope,
  so a host can replay it for cross-scope consolidation or sync. There is no way to filter it by
  scope, and for that reason it is rejected with `invalid_params` unless the server was started
  with `--allow-unscoped-changes`.

If you want per-conversation isolation, start a separate `topodb-mcp` process per conversation
with a distinct `--scope <ulid>` against the same `--db` file (or pass `scope`/`scopes`
explicitly on each tool call from a single server instance).

## v0 limitations

- **`Bytes` and `DateTime` prop values are unsupported over MCP.** Only string, integer, float,
  and bool prop values round-trip through JSON; attempting to write or a stored node that
  contains a `Bytes`/`DateTime` prop is rejected/errors rather than silently coerced.
- **No HTTP/SSE transport.** Only stdio, i.e. one client process per server process.
