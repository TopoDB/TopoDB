# topodb — Claude Code plugin

Persistent agent memory for Claude Code: a temporal property graph, scoped per
project with a shared layer for lessons that generalize. This plugin wires a
`topodb-mcp` server into Claude Code with no Rust toolchain and no manual
`.mcp.json` editing.

## Install

```
/plugin marketplace add TopoDB/TopoDB
/plugin install topodb
```

That's it. The next session in any project gets a `topodb` MCP server, a
`topodb-memory` skill that tells the agent when to recall and when to store,
and two slash commands: `/recall <query>` and `/remember <fact>`. Cursor
auto-imports this plugin; `launch.js` then uses the same data-dir rules as
the Cursor plugin (`CLAUDE_PLUGIN_DATA` when Claude Code set it, otherwise
the shared `~/.claude/plugins/data/topodb-topodb/` store) so import does not
advertise a 0-tool server.

### Requires `node` at runtime

The plugin is a Node launcher (`launch.js`) that connects to a shared
**broker** process, which spawns the real server, `@topodb/topodb-mcp`, as a
subprocess (see "How it works" below). `launch.js` downloads that server via
`npm` into the plugin's data directory on first run and reuses it after that
— no `cargo install`, no Rust toolchain. `npm` is resolved next to the `node`
that launched `launch.js` (editor MCP spawns often have `node` but not `npm`
on `PATH`). Node ≥ 22.19. This is the same constraint `@topodb/pi` has; if you
already run Pi extensions, you already satisfy it.

## What runs automatically

Three hook-driven behaviors, all failing silently to "nothing happens"
rather than ever blocking a session:

- **Context warehouse capture:** every `Read`/`Bash`/`Edit`/`Write`/`Grep`/
  `Glob`/`WebFetch` result (main session and subagents) is appended, raw, to
  a spool under `memory.warehouse/` next to the db, plus session start/end
  and memory-write markers. The daemon's hygiene tick drains the spool into
  content-addressed, redacted segments, mirrors the engine op log, and derives
  `Artifact`/`Chunk` nodes with `evidence` links to the memories each session
  wrote (search with `labels: ["Chunk"]`; `traverse` from a memory reaches its
  evidence). Deterministic, no model calls. `TOPODB_WAREHOUSE=0` turns just
  this off (`TOPODB_RECORDING=0` turns everything off); `TOPODB_WAREHOUSE_DIR`
  relocates it. The hooks also honour `[warehouse] path`/`enabled` in the
  nearest `.topodb.toml` above the plugin data dir (a well-formed file),
  exactly as the daemon does, so one setting steers both. See `topodb
  warehouse status`.
- **Session-start recall:** each new session (fresh start or `/clear`)
  begins with up to 8 recent memories for this project injected as
  context — ranked by access within the recent window, capped well under
  2k tokens. No broker running yet (the very first session of a project)
  means no injection; it appears from the next session on. The recall
  injection needs a 0.0.11+ server and the hygiene nudge below needs 0.0.12+;
  the plugin pins 0.0.15, so both are live (0.0.15 brings the vector-engine
  overhaul — deterministic HNSW ANN + SQ8 quantized storage, formats v7+v8,
  existing DB files migrate in place on first open; 0.0.14 brought the full F6
  memory lifecycle: `forget` tombstones, the `kind` taxonomy on
  remember/search, the `lifecycle_candidates` decay sweep, plus the
  obsidian vault-bridge tools; 0.0.13 had added `as_of` temporal
  reads on traverse/get_edges, an Entity down-weight default in
  search_memories, and text-fallback hygiene with honest `degraded`
  reporting when no embedder is available). When the
  store has accumulated cruft, the injection also carries a one-line
  **memory-health nudge** (`🧹 Memory hygiene: N duplicate pairs, N
  supersessions, N orphans, N stale …`) from a `memory_health` scan run
  concurrently with the recall — so an agent notices redundancy, contradicting
  facts, orphans, and cold memories at session start and can review with
  `memory_health` / the `find_*` scans, then `consolidate`/`link`/`supersede`.
  (Duplicates are the same fact reworded → merge; supersessions are pairs that
  contradict → retire the stale one.) Stale uses a 90-day window to stay
  meaningful; the nudge is advisory and, like everything here, fails silently
  to nothing.
- **Episode capture:** the plugin records which memories each
  `search_memories`/`traverse`/`recent_memories` call returned and, at
  session end, writes an `Episode` node with `RetrievalEvent`s marking
  which memories the session actually used (judged against the
  transcript). This is observational — no model calls — and it is the
  raw material future consolidation builds on. Set `TOPODB_RECORDING=0`
  to turn capture off. In-flight session state lives under
  `episodes/` in the plugin data dir and is swept after 7 days.

## How it works

redb, the database engine behind `memory.redb`, allows only one process to
hold the file open at a time. Claude Code runs one `topodb-mcp` per window, so
without help only the first window to open would get memory — every other
window's server would fail to open the database, near-silently.

To fix that, this plugin runs a single background **broker** process that
owns the database; every window's `launch.js` is a thin client that connects
to the broker over a local socket (a named pipe on Windows) instead of
opening the database itself. The broker is what makes memory work in every
window at once, not just the first.

The broker starts on demand — the first session to connect spawns it — and
exits about 60 seconds after the last window closes, releasing the database.
You do not start or stop it yourself.

**This means a background `node` process will be running whenever you have a
Claude Code window open with this plugin installed.** That's the broker; it's
expected, and it's how cross-window memory works. If you see an unfamiliar
`node` process in your task manager, this is almost certainly it.

If memory ever fails to come up, the broker's log is at
`<plugin-data>/broker.log` (the same directory `memory.redb` lives in — see
below).

## Memory model

Every session's reads span **this project's scope** plus a **`shared`**
scope; writes default to the project scope. The bundled skill tells the agent
to pass `scope: "shared"` explicitly when a fact generalizes beyond the
current repo — a preference in how you like to work, a lesson about a person
or service, anything that would be just as true in a different codebase.

`get_changes` — the one *unscoped* read topodb-mcp exposes, which replays the
op log across every scope in the database — is never enabled for this
launcher. A session can recall its project plus `shared`; it cannot list what
every other project has stored.

## Where the database lives

One file: `~/.claude/plugins/data/<plugin-id>/memory.redb`. `<plugin-id>` is
whatever id Claude Code assigns this plugin under your install (a
`--plugin-dir` dev install and a marketplace install get different ids); the
directory itself comes from `CLAUDE_PLUGIN_DATA`, which Claude Code sets and
which survives plugin updates.

There is exactly one `.redb` file, shared by every project you use this
plugin in — see the risk below. The [Cursor plugin](../cursor/README.md) uses
this same directory when it exists, so memory, scopes and the daemon are
shared across both editors.

## The risks, stated plainly

This design trades some isolation for the ability to recall across projects.
Two consequences are deliberate and worth knowing before you rely on this:

- **One global database across all projects.** Scopes keep a session's reads
  and writes confined to its own project plus `shared`, and `get_changes` is
  never turned on, so a session cannot enumerate or replay another project's
  memory. But it is still one file on disk: a corrupted database, a bad
  migration, or a bug in the shared server takes down memory for every
  project at once, not just the one you're working in. That blast radius is
  real and it is accepted in exchange for the cross-project `shared` scope —
  if you want hard per-project isolation instead, this plugin is not that.

- **The database grows with every session** unless `TOPODB_RECORDING=0`.
  Session-end episode capture writes nodes and edges to record which memories
  were retrieved — intended for consolidation, but adds disk growth even if
  no agent action is taken.
- **The warehouse grows with every tool call** (raw file reads and command output, redacted for known secret shapes, tiered and expired per `[warehouse]` in `.topodb.toml`) unless `TOPODB_WAREHOUSE=0`.

- **The scope is derived from the absolute project path, and that derivation
  is not portable.** The scope id is `ULID(sha256(canonical absolute project
  path))` — deterministic for one checkout, but two different checkouts of
  the *same* repository (a second clone on the same machine, or the same repo
  on a different machine) resolve to two different, unrelated scopes, with no
  merge between them. (An earlier version of this design's docs claimed the
  derivation was "reproducible across machines" — it is not, and that claim
  is wrong.) Because the database itself is local to the machine, this costs
  nothing in the common case — you only run into it if you expected the same
  memory to follow a repo across clones or machines, which it will not.

## Configuration

Set these environment variables to control plugin behavior:

- `TOPODB_RECORDING=0|off` — disable all session recording (episode capture,
  warehouse capture, and markers). This completely turns off the memory
  recording infrastructure for the session.
- `TOPODB_WAREHOUSE=0|off` — disable just warehouse capture and markers; keeps
  session recording enabled. Useful if you want episode capture but not the
  raw context warehouse.
- `TOPODB_WAREHOUSE_SPOOL_MAX_MB=<n>` — cap on the spool backlog (total bytes
  under `memory.warehouse/spool/`; default 64, `0` = unlimited). Over the cap,
  artifacts are dropped until the daemon drains the spool — markers still
  land — and one stderr line notes the first drop.
- `TOPODB_WAREHOUSE_DIR=<path>` — relocate the warehouse spool and sealed
  segments to a different directory (by default colocated with `memory.redb`).

  **Plugin hooks read `TOPODB_WAREHOUSE`, `TOPODB_WAREHOUSE_DIR`, and the nearest
  `.topodb.toml` above the plugin data dir** — `[warehouse].enabled`/`path` there
  govern the hooks and the daemon alike (env dir wins over toml path; toml
  `enabled = false` wins over everything). A *project* `.topodb.toml` does not
  apply to the plugin's db, which lives in the data dir.
- `TOPODB_CAPTURE_NUDGE=0|off` — suppress the stop-capture nudge (the
  suggestion to use `remember`/`create_memory` to ensure session artifacts
  are saved when closing a session without any explicit memory writes).
- `TOPODB_HOOK_DEBUG=1` (or a `HOOK_DEBUG` marker file in the plugin data
  dir: `touch ~/.claude/plugins/data/topodb-topodb/HOOK_DEBUG`) — make the
  retrieval `PostToolUse` hook dump the raw payload it received to
  `episodes/debug-last-payload.json` in the plugin data dir, so the true
  `tool_output`/`tool_response` shape can be pinned when debugging episode
  capture. The marker needs no restart; delete it when done. Dumps contain
  raw tool output. Nothing else reads this switch.

## What this plugin does not do

- No LLM consolidation, summarization, or decay in the hooks. Hooks run
  observational capture only (no `model_call`, no async agent). Session-start
  injection pulls what's already stored, ranked by recency; session-end episode
  capture judges what the transcript used — both fail silently if the broker is
  down. The hooks also do not capture in subagent sessions (only main, `startup`
  and `clear` sources). No automatic "forget" or summarize-old-memories pass.
  (`@topodb/pi`'s episode consolidation is a reference implementation; whether
  to bring a host-side consolidation loop here is open.)
- No embeddings configuration knob. The server the plugin launches runs
  embeddings **on by default** (`bge-small-en-v1.5`, downloaded once to
  `~/.cache/topodb/models`), so `search_memories` gets a semantic-recall leg
  in addition to text and graph. That requires an ONNX Runtime dynamic
  library on the host (e.g. `brew install onnxruntime`; the loader honors
  `ORT_DYLIB_PATH` if you point it at one directly) — without it, `db_info`
  reports embeddings `status: "failed"` and the plugin runs exactly as before,
  text+graph-only, with no other change in behavior. This plugin does not
  expose a way to pass `--embeddings off` or `--model-dir`; if you need that,
  run `topodb-mcp` yourself (see the main [`topodb-mcp` README](../../crates/topodb-mcp/README.md#cli-reference)).
- No CLI, no direct file access story beyond what `topodb-mcp` itself gives
  you. For scripting against a `.redb` file directly, see
  [`topodb-cli`](../../crates/topodb-cli/README.md) in the main repo.

## Server version

The server package (`@topodb/topodb-mcp`) is pinned by hand in
`server-args.js` (`SERVER_VERSION`), not resolved to "latest." That's
deliberate — a server whose tool surface moved under this plugin without a
matching update here is worse than one that's a version behind — but it also
means the pin can go stale if `topodb-mcp` publishes and this plugin doesn't
bump in step.

> **Release coordination.** When `topodb-mcp` publishes a new version, do
> NOT bump `SERVER_VERSION` until the package is actually on npm (bumping
> early would point every installed plugin at a version that doesn't
> exist), and re-verify `plugins/claude-code/test/broker.test.js` against
> the real published package as part of the bump. Each `topodb-mcp` release
> carries this as a checklist item in `CHANGELOG.md`.
