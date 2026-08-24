# topodb — Codex CLI plugin

Persistent agent memory for Codex CLI: a temporal property graph, scoped per
project with a shared layer for lessons that generalize. Same engine and same
behavior as the [Claude Code plugin](../claude-code/README.md) and the
[Cursor plugin](../cursor/README.md); if more than one is installed they
share one database and one daemon.

## Install

- **From this repo (git marketplace):**

  ```
  codex plugin marketplace add https://github.com/TopoDB/TopoDB
  codex plugin add topodb
  ```

  The root `.agents/plugins/marketplace.json` lists the `topodb` plugin at
  `plugins/codex`. (Codex also legacy-reads `.claude-plugin/marketplace.json`,
  which points at the Claude Code plugin — that works as a compat bonus, but
  this native entry is the supported path.)
- **Without the plugin system:** register the MCP server directly and copy the
  skill:

  ```
  codex mcp add topodb -- node /path/to/TopoDB/plugins/codex/launch.js
  cp -r plugins/codex/skills/topodb-memory ~/.agents/skills/topodb-memory
  ```

  You get the memory tools and the skill, but none of the automatic hooks
  below (no session-start recall, no capture, no episode flush).

`launch.js` downloads `@topodb/topodb-mcp` into the plugin data directory on
first run. Node ≥ 22.19.

## The `/hooks` trust step

After installing, Codex will ask you — once, in `/hooks` — to approve the
plugin's five hook commands. This is Codex's trust model, and it is worth
understanding what you are approving and why the prompt looks the way it
does:

- **What you approve:** five command hooks, each exactly
  `node ${PLUGIN_ROOT}/hooks/<name>.js` — `session-start.js` (twice: once for
  startup/resume/clear, once for post-compaction re-injection),
  `warehouse-capture.js`, `stop.js`, and `session-end.js`. No flags, no
  version strings, no environment variables in the definitions; the only
  interpolation is `${PLUGIN_ROOT}`. What each one does is described in the
  next section, and all of them fail silently to "nothing happens" — none
  can block Codex.
- **Why it's one-time:** Codex trust-hashes each hook *definition*. Because
  ours are frozen to those exact static strings, routine plugin updates —
  bug fixes, behavior changes, new server versions — change only the script
  *contents*, never the definitions, so they do **not** re-prompt.
- **When it re-prompts:** any update that touches `hooks.json` itself (a new
  hook, a changed matcher) changes the hash and Codex will ask again in
  `/hooks`. That is expected, rare, and called out in release notes when it
  happens — a re-prompt you didn't expect is worth a second look.

## What runs automatically

All hook-driven, all failing silently to "nothing happens", never blocking:

- **Session-start recall** (`SessionStart`, matchers `startup|resume|clear`
  and `compact`): recent project memories plus a one-line memory-hygiene
  nudge are injected as `additionalContext`. The `compact` matcher re-fires
  the same script after compaction, so recall survives context compaction —
  what got squeezed out is re-injected. Background agents are skipped.
- **Context warehouse capture** (`PostToolUse`, async, all tools): tool
  results are spooled raw under `memory.warehouse/` next to the db
  (`apply_patch` arrives pre-normalized as Edit/Write diffs; shell commands,
  file reads, and MCP tool calls are captured too); the daemon's hygiene tick
  drains, redacts, and derives `Artifact`/`Chunk` nodes
  (`topodb warehouse status`). `TOPODB_WAREHOUSE=0` turns just this off.
  `TOPODB_WAREHOUSE_SPOOL_MAX_MB` (default 64) caps the spool backlog; over
  it, artifacts are dropped (markers still land) until the next drain.
- **Episode flush** (`Stop`): what `search_memories` returned and which of it
  the session used, written as an `Episode` at turn end. Stop's 600 s budget
  makes this the load-bearing flush; if the daemon is unreachable the state
  is kept for a later sweep, never dropped.
- **Capture nudge** (`Stop`): once per substantive session (≥5 tool calls, no
  memory written yet) a follow-up asks the agent to `remember` durable facts.
  Loop-guarded via `stop_hook_active`. `TOPODB_CAPTURE_NUDGE=0` turns only
  this off.
- **Session-end marker** (`SessionEnd`): writes a terminal marker and spawns
  a detached flusher, exiting well inside Codex's 3 s hook cap. Advisory
  only — Stop already flushed; this also fires after 30 min idle.

`TOPODB_RECORDING=0` turns everything off. Plus the `topodb-memory` skill
(`skills/topodb-memory/SKILL.md`, the agent-skills standard — Codex has no
rules files; the skill carries the memory discipline).

## Codex's native Memories feature

Codex ships its own off-by-default "Memories" feature
(`[features] memories = true` in `config.toml`). It solves a narrower
problem — automatic summaries of past chats — where TopoDB is a queryable
temporal graph: entities, typed edges, supersession history, cross-project
`shared` scope, and explicit recall tools the agent can search and traverse.

Our recommendation: **leave native Memories off while using TopoDB.** Two
memory systems injecting context independently means duplicated, sometimes
disagreeing recall. If you do enable it, note that Codex's own default
`memories.disable_on_external_context` already excludes MCP-using chats from
memory generation — so the two mostly stay out of each other's way, but
TopoDB is designed to be the memory layer, not a supplement to one.

## Where the database lives

Resolved once per server start, first match wins:

1. `TOPODB_PLUGIN_DATA` (explicit override);
2. `PLUGIN_DATA` / `CLAUDE_PLUGIN_DATA` (Codex sets both; the latter is its
   explicit Claude-compat pair);
3. `~/.claude/plugins/data/topodb-topodb/` if it exists — the Claude Code
   plugin's store, so both CLIs share memory, scopes and daemon;
4. `~/.topodb/plugin-data/`.

`launch.js` prints `topodb: data dir <path> (<reason>)` on stderr; `db_info`
reports the path. Rule 3 keys on the default Claude Code marketplace id
(`topodb-topodb`); if you installed the Claude Code plugin under another
marketplace name, point both at one directory with `TOPODB_PLUGIN_DATA`.

## How it works

redb allows one process per database file, and Codex starts one MCP server
per session, so `launch.js` is a thin stdio client of a shared daemon
(`topodb-mcp --socket`) that owns `memory.redb`; the first session spawns it,
it exits shortly after the last one disconnects. If that socket never binds,
`launch.js` runs `topodb-mcp` on this session's stdio instead of advertising
zero tools. The daemon log is `<data dir>/daemon.log`. Scope = hash of the
project directory, identical to the Claude Code and Cursor plugins; reads
span the project plus `shared`, writes default to the project.

## Known gaps (Codex)

- Hook payload shapes are taken from Codex's docs and are **not yet pinned
  against a real session**. To capture the real shapes: set
  `TOPODB_HOOK_DEBUG=1`, use the agent, then read
  `<data dir>/episodes/debug-<event>.json`; unset it when done — the dumps
  contain raw tool output (file contents).
- `UserPromptSubmit` per-turn retrieval is not wired yet (promising
  follow-up).
- npm-source marketplace distribution and the OpenAI plugin directory are
  deferred; install from this repo for now.
