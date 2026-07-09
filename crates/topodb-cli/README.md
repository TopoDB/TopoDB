# topodb-cli

A direct-embedded, script-friendly command-line interface over a [TopoDB](https://crates.io/crates/topodb)
agent-memory database file. JSON in, JSON out, predictable exit codes — no server process, no
network hop.

Status: **v1** — the full read/write surface (11 commands). See [v1 limitations](#v1-limitations).

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
| `--pretty` | no | off | Pretty-print the JSON output instead of compact one-line JSON. |

## Commands

All 11 subcommands, in scaffold + write + read order:

| Command | Key flags | Output |
|---|---|---|
| `info` | — | `{"path","format_version","current_seq","index_spec","default_scope"}` |
| `create-memory` | `--content <text>` (required), `--props <json-object>` | `{"id": "<ulid>"}` |
| `create-entity` | `--name <text>` (required), `--props <json-object>` | `{"id": "<ulid>"}` |
| `link` | `--from <id>`, `--to <id>`, `--type <ty>` (all required), `--props <json-object>`, `--valid-from <unix-ms>` | `{"id": "<ulid>"}` |
| `get <id>` | positional node id | `{"found": bool, "node"?: {...}}` |
| `find` | `--label <l>`, `--prop <p>`, `--value <v>` (all required) | `[ node, ... ]` |
| `search <query>` | positional query, `--k <n>` (default 10) | `[ {"node":..., "score": f}, ... ]` |
| `traverse <seed>` | positional seed id, `--max-hops <n>` (default 2), `--direction out\|in\|both` (default `both`), `--edge-type <ty>` (repeatable) | `{"subgraph": {"nodes":[...],"edges":[...]}}` |
| `stats <id>` | positional node id | `{"found": bool, "access_stats"?: {"access_count","last_accessed_at"}}` |
| `changes` | `--since <seq>` (required) | `[ {"seq": u64, "op": <op-json>}, ... ]` |
| `compact` | `--keep-from <seq>` (required) | `{"oldest": <seq>}` |

Notes on individual commands:

- **`create-memory`**/**`create-entity`**: `--props` is a JSON *object* string merged in
  alongside the reserved key (`content` / `name`); a `--props` that tries to set the reserved
  key itself is rejected (exit 2), it never silently overwrites.
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

## Exit-code contract

| Code | Meaning |
|---|---|
| `0` | Success — **including** `get`/`stats` reporting `{"found": false}` for a missing or out-of-scope id. Not-found is a normal `Option` result, not an error. |
| `2` | Rejected / bad input: a clap usage error (missing `--db`, unknown flag/subcommand), a malformed `--scope`/`--props`/`--value`, an unparseable node id, or an engine `TopoError::Rejected` (undeclared index, empty batch, malformed query, `Compacted` changes range). |
| `1` | Internal / storage / db-open failure: anything the caller can't fix by changing their input — a missing parent directory for `--db`, a corrupt/incompatible file, or any non-`Rejected` `TopoError` variant. |

On failure, stderr carries `{"error": {"kind": "rejected"|"internal", "message": "..."}}`; stdout
is left empty. clap's own usage errors print clap's own message (not this JSON shape) but still
exit 2.

## Scoping

- The default scope (`--scope`, default `shared`) applies to every command except `changes`.
- `changes` is deliberately **unscoped**: the op log spans every scope, so a host can replay it
  for cross-scope consolidation. There's no way to filter it by scope on this CLI.
- There's no per-command `--scope` override in v1 — every invocation targets exactly one scope.
  To touch a different scope, pass a different global `--scope` on that invocation.

## v1 limitations

- **No vector search.** `search` is BM25 full-text only; there's no way to submit a raw vector
  query from the CLI.
- **No `set-props` / `remove-node`.** The write surface is create-only (`create-memory`,
  `create-entity`, `link`); mutating or deleting an existing node isn't exposed. Corrections go
  through a fresh fact (TopoDB facts supersede, they don't overwrite).
- **No bulk/stdin `submit`.** Each invocation does exactly one op; there's no way to pipe a batch
  of ops in over stdin. Scripting a bulk load means one process per op today.
- **Direct-embedded only, single-process access.** There's no `--connect`/HTTP mode — the CLI
  opens the `.redb` file directly in-process, the same way `topodb-mcp` does. That means you
  can't run `topodb` against a database file that another process (another `topodb` invocation,
  or a running `topodb-mcp` server) currently has open; opening will fail as a db-open error
  (exit 1). Point the CLI at the file only when nothing else has it open, or use a separate
  scope/file per concurrent consumer.

## Examples

Fresh database, `info`:

```console
$ topodb --db demo.redb info
{"current_seq":0,"default_scope":"shared","format_version":1,"index_spec":{"equality":[{"label":"Entity","prop":"name"}],"text":[{"label":"Memory","prop":"content"}]},"path":"demo.redb"}
```

Create an entity and a memory, then search for it:

```console
$ topodb --db demo.redb create-entity --name ada
{"id":"01KX2NZY1CCS7GVF59C8H909GG"}

$ topodb --db demo.redb create-memory --content "ada wrote the first program"
{"id":"01KX2NZY4VH5QQC16VHXHJSKFE"}

$ topodb --db demo.redb search "first program"
[{"node":{"id":"01KX2NZY4VH5QQC16VHXHJSKFE","label":"Memory","props":{"content":"ada wrote the first program"},"scope":"shared"},"score":0.5753642320632935}]
```

`get` on an id that doesn't exist — still exit 0:

```console
$ topodb --db demo.redb get 01ARZ3NDEKTSV4RRFFQ69G5FAV
{"found":false}
$ echo $?
0
```
