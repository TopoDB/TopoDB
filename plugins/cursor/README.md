# topodb — Cursor plugin

Persistent agent memory for Cursor: a temporal property graph, scoped per
project with a shared layer for lessons that generalize. Same engine and same
behavior as the [Claude Code plugin](../claude-code/README.md); if both are
installed they share one database and one daemon.

## Install

- **From this repo (team marketplace / "Import from Repo"):** add
  `https://github.com/TopoDB/TopoDB` — the root `.cursor-plugin/marketplace.json`
  lists the `topodb` plugin at `plugins/cursor`.
- **Local development:** `ln -s /path/to/TopoDB/plugins/cursor ~/.cursor/plugins/local/topodb`,
  then enable it under Customize → Plugins.
- Official Cursor Marketplace listing: pending submission.

### Requires `node` (and `npm`) on PATH

`launch.js` downloads `@topodb/topodb-mcp` into the plugin data directory on
first run and reuses it — no Rust toolchain. Node ≥ 22.19.

## What runs automatically

All hook-driven, all failing silently to "nothing happens", never blocking:

- **Chat-start recall** (`sessionStart`): up to 8 recent project memories
  (ranked by access) plus a one-line memory-hygiene nudge are injected as
  context. Background agents are skipped.
- **Context warehouse capture** (`postToolUse`): `Shell`/`Read`/`Write`/`Edit`/
  `Grep`/`Glob`/`WebFetch` results are spooled raw under `memory.warehouse/`
  next to the db; the daemon's hygiene tick drains, redacts, and derives
  `Artifact`/`Chunk` nodes (`topodb warehouse status`). `TOPODB_WAREHOUSE=0`
  turns just this off (`TOPODB_RECORDING=0` turns everything off).
- **Episode capture** (`afterMCPExecution` + `sessionEnd`): what
  `search_memories`/`traverse`/`recent_memories` returned and which of it the
  session used, written as an `Episode` at session end. `TOPODB_RECORDING=0`
  turns episode capture, the stop nudge, and warehouse capture off.
- **Stop nudge** (`stop`): once per substantive session (≥5 tool calls, no
  memory written yet) a follow-up asks the agent to `remember` durable facts.
  `TOPODB_CAPTURE_NUDGE=0` turns only this off.

Plus a rule (`rules/topodb-memory.mdc`, always applied), the `topodb-memory`
skill, and `/recall`, `/remember` commands.

## Where the database lives

Resolved once per server start, first match wins:

1. `TOPODB_PLUGIN_DATA` (explicit override);
2. `CLAUDE_PLUGIN_DATA` (only set inside Claude Code);
3. `~/.claude/plugins/data/topodb-topodb/` if it exists — the Claude Code
   plugin's store, so both editors share memory, scopes and daemon;
4. `~/.topodb/plugin-data/`.

`launch.js` prints `topodb: data dir <path> (<reason>)` on stderr; `db_info`
reports the path. Installing Claude Code later switches a Cursor-only install
from 4 to 3 on the next start — move or point with `TOPODB_PLUGIN_DATA` if you
want to keep the old store.

## How it works

redb allows one process per database file, and Cursor starts one MCP server per
window, so `launch.js` is a thin stdio client of a shared daemon
(`topodb-mcp --socket`) that owns `memory.redb`; the first window spawns it, it
exits shortly after the last one disconnects. The daemon log is
`<data dir>/daemon.log` (or `broker.log` on older servers). Scope = hash of the
workspace's first root path, identical to the Claude Code plugin; reads span the
project plus `shared`, writes default to the project.

## Known gaps (Cursor)

- No subagent recall injection (Cursor's `subagentStart` hook cannot return
  context); subagent tool calls are still captured.
- Multi-root workspaces use the first root for scope.
- Cloud agents: `sessionStart`/`sessionEnd`/`afterMCPExecution` don't fire
  there — capture and the stop nudge still do.
- Payload shapes (`tool_output`, transcript JSONL) were documented, then pinned
  in dogfood; set `TOPODB_HOOK_DEBUG=1` to dump raw payloads to
  `<data dir>/episodes/debug-<event>.json` if something looks off.
