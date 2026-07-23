# topodb-cli

A direct-embedded, script-friendly command-line interface over a [TopoDB](https://crates.io/crates/topodb)
agent-memory database file. JSON in, JSON out, predictable exit codes — no server process, no
network hop.

Status: **v1** — the full read/write surface (17 commands). See [v1 limitations](#v1-limitations).

## Install

```bash
cargo install topodb-cli
```

This installs a binary named **`topodb`** (not `topodb-cli`) to your Cargo bin directory
(typically `~/.cargo/bin`), which must be on `PATH`.

## Global flags

```
topodb --db <path> [--scope <ulid|shared>] [--pretty] <command> [args...]
```

| Flag | Required | Default | Meaning |
|---|---|---|---|
| `--db <path>` (env `TOPODB_DB`) | yes | — | Path to the redb database file. A missing file is created fresh (with the canonical default index spec — equality on `Entity/name`, text on `Memory/content`); an existing file is opened with **its own persisted index spec** via `Db::open_stored` — no `--spec` flag exists on this CLI, and none is ever needed. A missing *parent directory* is a db-open failure. |
| `--scope <ulid\|shared>` | no | `shared` | The default scope every scoped command uses. `"shared"` (case-insensitive) resolves to the shared scope; any other value is parsed as a `ScopeId` ULID. An invalid value is rejected before the db is even opened. |
| `--lock-wait-ms <ms>` (env `TOPODB_LOCK_WAIT_MS`) | no | `3000` | How long to retry on lock contention (`TopoError::Busy`) during database open. `0` disables retries and fails immediately. See **Exit-code contract** below for the exit code on lock exhaustion. |
| `--pretty` | no | off | Pretty-print the JSON output instead of compact one-line JSON. |

## Commands

All 17 subcommands, in scaffold + write + read order:

| Command | Key flags | Output |
|---|---|---|
| `info` | — | `{"path","format_version","current_seq","index_spec","default_scope"}` |
| `create-memory` | `--content <text>` (required), `--props <json-object>`, `--scope <ulid\|shared>` | `{"id": "<ulid>", "deduplicated": bool}` |
| `create-entity` | `--name <text>` (required), `--props <json-object>`, `--scope <ulid\|shared>`, `--always-create` | `{"id": "<ulid>", "created": bool}` |
| `remember` | `--content <text>` (required), `--entity <name>` (required, repeatable), `--edge-type <ty>` (default `"about"`), `--supersedes <id>` (repeatable), `--props <json-object>`, `--scope <ulid\|shared>` | `{"memory_id": "<ulid>", "deduplicated": bool, "entities": [{"name": "<name>", "id": "<ulid>", "created": bool}], "edge_ids": ["<ulid>", ...], "superseded": ["<ulid>", ...]}` |
| `link` | `--from <id>`, `--to <id>`, `--type <ty>` (all required), `--props <json-object>`, `--valid-from <unix-ms>`, `--scope <ulid\|shared>` | `{"id": "<ulid>"}` |
| `get <id>` | positional node id | `{"found": bool, "node"?: {...}}` |
| `find` | `--label <l>`, `--prop <p>`, `--value <v>` (all required) | `[ node, ... ]` |
| `search <query>` | positional query, `--k <n>` (default 10) | `[ {"node":..., "score": f}, ... ]` |
| `traverse <seed>` | positional seed id, `--max-hops <n>` (default 2), `--direction out\|in\|both` (default `both`), `--edge-type <ty>` (repeatable) | `{"subgraph": {"nodes":[...],"edges":[...]}}` |
| `stats <id>` | positional node id | `{"found": bool, "access_stats"?: {"access_count","last_accessed_at"}}` |
| `changes` | `--since <seq>` (required) | `[ {"seq": u64, "op": <op-json>}, ... ]` |
| `compact` | `--keep-from <seq>` (required) | `{"oldest": <seq>}` |
| `set-props <id>` | positional node id, `--props <json-object>` (required; a `null` value removes that key) | `{"seq": <seq>}` |
| `remove-node <id>` | positional node id | `{"seq": <seq>}` |
| `close-edge <id>` | positional edge id, `--valid-to <unix-ms>` (defaults to "now") | `{"seq": <seq>}` |
| `set-embedding <id>` | positional node id, `--model <name>`, `--vector <json-float-array>` (both required) | `{"seq": <seq>}` |
| `search-vector` | `--model <name>`, `--vector <json-float-array>` (both required), `--k <n>` (default 10), `--candidate <id>` (repeatable) | `[ {"node":..., "score": f}, ... ]` |
| `submit [input]` | positional path to a JSON command array, or `-`/omitted for stdin | `{"ids": [...]}` |

Notes on individual commands:

- **`create-memory`**: stamped with a `content_hash` (stored as a node property for dedup tracking). If identical content
  (after whitespace normalization) was already stored, `"deduplicated": true` and the existing
  memory id is returned; `--props` is ignored on a hit. `"deduplicated": false` indicates a new
  memory. The response is `{"id":…,"deduplicated":…}`. `--props` is a JSON *object* string merged in alongside `content`; a `--props` that
  tries to set `content` itself is rejected (exit 2). The system-maintained keys `content_hash` and `superseded_at` are reserved — attempts to set them are rejected (exit 2).
- **`create-entity`**: now find-or-create by default — the name is matched case- and
  whitespace-insensitively across write scope and `shared`, and resolves aliases.
  An existing entity is returned with `"created": false`; `--always-create` restores the old
  raw-create behavior. Both paths now report the `created` flag. When `created: false`, `--props`
  merges only NEW keys; a `name` key in props is rejected either way (exit 2).
- **`remember`**: atomic store-and-link — one call creates a memory (with dedup), find-or-creates
  each named entity (same semantics as `create-entity`), and links them. Combines three operations
  into a single engine batch so facts never strand unlinked. `--entity` is repeatable (must pass
  ≥1). `--supersedes` is repeatable and marks the listed memory ids as superseded. `--edge-type`
  defaults to `"about"` and describes the link from memory→entity. The system-maintained keys `content_hash` and `superseded_at` are reserved — attempts to set them in `--props` are rejected (exit 2). Re-storing identical content from a superseded memory creates a new memory instead of deduping to the retired tombstone.
- **`link`**: `--valid-from` is Unix milliseconds; omit it to let the engine resolve "now".
- **`find`**: `--value` is parsed as a JSON scalar first (`42` → `Int`, `true` → `Bool`, `"ada"`
  → `Str`); if it doesn't parse as JSON at all, the raw string is taken as `Str` — so
  `--value ada` and `--value '"ada"'` are equivalent. A float value is never equality-indexable
  and comes back rejected. `find` errors (exit 2) if `(label, prop)` isn't declared in the open
  db's index spec.
- **`traverse`**: omitting `--edge-type` entirely follows every edge type; passing it (once or
  repeated) restricts the walk to exactly those types.
- **`changes`**: the one unscoped command — see [Scoping](#scoping) — and `Compacted` (the
  requested `--since` is below the retained floor) is a rejected/exit-2 condition, not an
  internal error; the caller re-anchors from `info`'s `current_seq` rather than trusting a
  truncated tail.
- **`compact`**: drops every op-log entry with `seq < --keep-from`.
- **`submit [input]`**: submit a JSON array of batch commands atomically. Each command has an `op` field (the command type) and type-specific fields. `#N` in an id field is 0-indexed: `#0` refers to the first command's produced id, `#1` to the second command's produced id, etc. Reads from `input` file path, or stdin if `-` or omitted. This matches `submit_batch` in `topodb-mcp`.

## Exit-code contract

| Code | Meaning |
|---|---|
| `0` | Success — **including** `get`/`stats` reporting `{"found": false}` for a missing or out-of-scope id. Not-found is a normal `Option` result, not an error. |
| `3` | Lock exhaustion — database open timed out after `--lock-wait-ms` retries on `TopoError::Busy` (another process/client held the database lock for the entire retry window). Caller may increase `--lock-wait-ms` and retry. |
| `2` | Rejected / bad input: a clap usage error (missing `--db`, unknown flag/subcommand), a malformed `--scope`/`--props`/`--value`, an unparseable node id, or an engine `TopoError::Rejected` (undeclared index, empty batch, malformed query, `Compacted` changes range). |
| `1` | Internal / storage / db-open failure: anything the caller can't fix by changing their input — a missing parent directory for `--db`, a corrupt/incompatible file, or any non-`Rejected`/`Busy` `TopoError` variant. |

On failure, stderr carries `{"error": {"kind": "busy"|"rejected"|"internal", "message": "..."}}`; stdout
is left empty. clap's own usage errors print clap's own message (not this JSON shape) but still
exit 2.

## Scoping

**A write is stamped with exactly one scope; a read filters by a set.** That asymmetry is the
model, not an oversight.

- The global `--scope` (default `shared`) supplies the default for every command except `changes`.
- **Per-command `--scope` override.** The three commands that *stamp* a scope — `create-memory`,
  `create-entity`, `link` — each take their own `--scope <ulid|shared>`, overriding the global one
  for that invocation. This matches `submit`'s batch DSL (whose `create_memory`, `create_entity`,
  and `link` ops each take an optional `scope` field) and the `topodb-mcp` tools of the same names.
- `set-props`, `remove-node`, `close-edge`, and `set-embedding` address an existing node or edge
  **by id** and stamp no scope of their own, so they take no `--scope`.
- **`link --scope` is what keeps shared memories connected.** An edge created while `--scope` names
  a project is stamped with that project, so it is invisible from every other project — the nodes
  would be shared but disconnected. Pass `--scope shared` on a `link` between `shared` nodes.
- `changes` is deliberately **unscoped**: the op log spans every scope, so a host can replay it for
  cross-scope consolidation. There's no way to filter it by scope on this CLI.

### Why `changes` isn't gated here, but `get_changes` is on `topodb-mcp`

`topodb-mcp` serves `get_changes` only when started with `--allow-unscoped-changes`. This CLI needs
no such flag. The difference is deliberate.

**The MCP gate prevents accidents, not attackers.** MCP *advertises* `get_changes` in the model's
tool list, so an agent can trip over it while doing something else and replay every other project's
writes into its own context. This CLI advertises nothing to a model: reaching `changes` takes
deliberate intent, and whoever can run `topodb --db <file> changes` already holds the file and could
read it directly.

**Accepted risk, stated plainly:** an agent with shell access bypasses the MCP gate entirely by
invoking this CLI against the same database file. `--allow-unscoped-changes` is not a security
boundary and nothing here pretends otherwise. If a future host drives this CLI from an agent loop,
that decision needs revisiting.

## v1 limitations

- **No `--spec` flag.** An existing db is always opened with its own persisted index spec; a fresh
  one is created with the canonical default (equality on `Entity/name`, text on `Memory/content`).
  There's no way to declare a different spec from this CLI.
- **No multi-scope reads.** Every read filters by the single scope named in the global `--scope`.
  `topodb-mcp` can read across a *set* of scopes (`--read-scopes`, and a `scopes` param on its read
  tools); this CLI cannot. To read another scope, run again with a different `--scope`.
- **Direct-embedded only, single-process access.** There's no `--connect`/HTTP mode — the CLI opens
  the `.redb` file directly in-process, the same way `topodb-mcp` does. You can't run `topodb`
  against a database file another process (another `topodb` invocation, or a running `topodb-mcp`
  server) currently has open; opening fails as a db-open error (exit 1). Point the CLI at the file
  only when nothing else has it open, or use a separate file per concurrent consumer.

> The removed bullets ("No vector search", "No `set-props`/`remove-node`", "No bulk/stdin `submit`")
> were all false. `search-vector`, `set-props`, `remove-node`, and `submit` exist.

## Examples

Fresh database, `info`:

```console
$ topodb --db demo.redb info
{"current_seq":0,"default_scope":"shared","format_version":6,"index_spec":{"equality":[{"label":"Entity","prop":"name"},{"label":"Alias","prop":"name"},{"label":"Synonym","prop":"term"},{"label":"Memory","prop":"content_hash"}],"text":[{"label":"Memory","prop":"content"},{"label":"Entity","prop":"name"},{"label":"Alias","prop":"name"}]},"path":"demo.redb"}
```

Create an entity and a memory, then search for it:

```console
$ topodb --db demo.redb create-entity --name ada
{"created":true,"id":"01KX2NZY1CCS7GVF59C8H909GG"}

$ topodb --db demo.redb create-memory --content "ada wrote the first program"
{"deduplicated":false,"id":"01KX2NZY4VH5QQC16VHXHJSKFE"}

$ topodb --db demo.redb search "first program"
[{"node":{"id":"01KY6TA0DC8YYBKMW9XJASEQ07","label":"Memory","props":{"content":"ada wrote the first program","content_hash":"ba78ad81d00f6917"},"scope":"shared"},"score":0.5753642320632935}]
```

`get` on an id that doesn't exist — still exit 0:

```console
$ topodb --db demo.redb get 01ARZ3NDEKTSV4RRFFQ69G5FAV
{"found":false}
$ echo $?
0
```
