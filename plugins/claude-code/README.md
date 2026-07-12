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

The plugin is a Node launcher (`launch.js`) that spawns the real server,
`@topodb/topodb-mcp`, as a subprocess. It downloads that server via `npm`
into the plugin's data directory on first run and reuses it after that — no
`cargo install`, no Rust toolchain — but it does need a working `node` (and
`npm`) on `PATH`. This is the same constraint `@topodb/pi` has; if you already
run Pi extensions, you already satisfy it.

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

The server package (`@topodb/topodb-mcp`) is pinned by hand in `launch.js`
(`SERVER_VERSION`), not resolved to "latest." That's deliberate — a server
whose tool surface moved under this plugin without a matching update here is
worse than one that's a version behind — but it also means the pin can go
stale if `topodb-mcp` publishes and this plugin doesn't bump in step.
