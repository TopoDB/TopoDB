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
and two slash commands: `/recall <query>` and `/remember <fact>`.

### Requires `node` at runtime

The plugin is a Node launcher (`launch.js`) that connects to a shared
**broker** process, which spawns the real server, `@topodb/topodb-mcp`, as a
subprocess (see "How it works" below). `launch.js` downloads that server via
`npm` into the plugin's data directory on first run and reuses it after that
— no `cargo install`, no Rust toolchain — but it does need a working `node`
(and `npm`) on `PATH`. This is the same constraint `@topodb/pi` has; if you
already run Pi extensions, you already satisfy it.

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
plugin in — see the risk below.

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

## What this plugin does not do

- No hooks, no automatic episode recording. Recall and storage are both
  explicit — the agent decides to call `search_memories` / `create_memory`
  (guided by the bundled skill), nothing runs on a session-lifecycle hook.
  (`@topodb/pi`'s episode recorder has since shipped there; whether to bring
  the equivalent here is open, tracked separately from this plugin.)
- No vector search / embeddings configuration. The server supports it;
  this plugin does not expose setup for it.
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
