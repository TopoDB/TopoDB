# Changelog

All notable changes to the packages in this repository are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Packages in this
workspace are versioned and released independently (tags are per-package, e.g.
`topodb-mcp-v0.0.4`), so each package has its own section below.

> **This changelog starts at the releases below.** Earlier versions predate it and are not
> reconstructed here — the git history is the record for those. A changelog that guesses at its
> own past is worse than one that says where it begins.

---

## `topodb-mcp`

### 0.0.4 — unreleased

#### Breaking

- **`get_changes` now requires the server to be started with `--allow-unscoped-changes`.**
  Without the flag it returns `invalid_params`. **Any existing client that calls `get_changes`
  breaks on upgrade from 0.0.3.**

  `get_changes` is the one unscoped read: the op log spans every scope in the database. In a
  database shared across projects — which is what the forthcoming Claude Code plugin creates —
  an agent calling `get_changes(since_seq: 0)` replays every *other* project's writes into its
  own context. That is cross-project contamination and a token bomb, and before this change it
  was reachable by accident rather than by choice.

  **Migration:** if you genuinely need the op log (sync and consolidation hosts do), start the
  server with `--allow-unscoped-changes`. Scope-*filtering* the log was considered and rejected:
  a partial log cannot be replayed deterministically, which would break the tool's actual
  contract.

#### Added

- **`--read-scopes <list>`** — a comma-separated list of `shared` / scope-ULID entries defining
  the server's default **read** `ScopeSet`. Defaults to the single value of `--scope`, so the
  single-scope behaviour every existing client relies on is preserved exactly.

  `--scope` remains the default **write** scope and is unchanged. Two flags, because a read
  filters by a *set* and a write picks exactly *one* scope — overloading a single flag with both
  meanings would make `--scope shared,<ulid>` and `--scope <ulid>,shared` differ invisibly.
- **`scopes: string[]`** — an optional param on the six read tools (`get_node`, `find_by_prop`,
  `search_memories`, `traverse`, `access_stats`, `search_vectors`), building a genuine
  multi-member `ScopeSet`. Precedence: `scopes` > `scope` > the server's default read set.

  Before this, no client could read across two scopes at all — "this project **plus** `shared`"
  was unexpressible, even though `ScopeSet` is the engine's central read type.
- **`scope`** on the `link` tool and on the batch DSL's `link` op, so an edge can be stamped with
  a scope other than the server's default write scope. Without it, an edge attached to a `shared`
  node while the default write scope was a project would be project-scoped and invisible from
  every other project: shared memories would become disconnected islands, with `search_memories`
  still surfacing the node's text while `traverse` silently failed to cross.

#### Fixed

- **Write tools silently accepted and ignored a `scopes` param.** `create_memory` with
  `{"scopes": ["shared"]}` returned success and wrote to the *project* scope. All 15 param
  structs now reject unknown fields (`#[serde(deny_unknown_fields)]`), so this is an error
  instead of a lie.
- **`db_info` reported only the write scope, not the read set.** An agent following the server's
  own instructions would pass `scope: "shared"` on a read, which **narrows** the read set and
  silently drops every project result. `db_info` now reports the default read scopes.

---

## `topodb-cli`

### 0.0.2 — unreleased

#### Added

- **`--scope <ulid|shared>` on `create-memory`, `create-entity`, and `link`** — a per-command
  override of the global `--scope`, for the three commands that stamp a scope.

  These are the same three ops `submit`'s batch DSL scopes per-op, so the
  CLI's two ways to write now agree. `link --scope shared` in particular is what lets a `shared`
  edge join two `shared` nodes; without it the edge takes the global scope and is invisible from
  every other project.

  `set-props`, `remove-node`, `close-edge`, and `set-embedding` address an existing node or edge
  by id and stamp no scope, so they take no `--scope`.

#### Changed

- `changes` is documented as deliberately **ungated**, unlike `topodb-mcp`'s `get_changes`. The
  MCP gate stops an LLM tripping over an advertised tool; it prevents accidents, not attackers.
  The bypass — an agent with shell access invoking this CLI against the same database file — is
  recorded as an accepted risk rather than left implicit.
- Corrected a materially stale README: it claimed the CLI had no vector search, no
  `set-props`/`remove-node`, and no batch `submit` (all four exist), and counted 11 commands when
  there are 17.
