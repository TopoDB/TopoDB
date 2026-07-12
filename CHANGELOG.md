# Changelog

All notable changes to the packages in this repository are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Packages in this
workspace are versioned and released independently (tags are per-package, e.g.
`topodb-mcp-v0.0.4`), so each package has its own section below.

> **This changelog starts at the releases below.** Earlier versions predate it and are not
> reconstructed here — the git history is the record for those. A changelog that guesses at its
> own past is worse than one that says where it begins.

---

## `topodb` (engine)

### Unreleased

#### Breaking

- **On-disk format v4** ([FORMAT.md](FORMAT.md)): clustered vector storage — `vectors`/`embedding_ref`/
  `vector_dims` replace the old slot-keyed `embeddings` cold table — and a chunked full-text postings
  layout (`postings` re-keyed from one row per term to `[scope][term][chunk]`, ~8 KiB per chunk). See
  "Fixed" below for why the postings change matters in practice.
- **ONE-WAY auto-migration of v1/v2/v3 files on open, now chained all the way through v4.** An
  existing v1, v2, or v3 database file is migrated to v4 automatically the first time it's opened
  with this version — there is no path back, same one-way contract 0.0.6 established for v1/v2 → v3,
  extended one hop further. A v3 file whose `embeddings` table happens to record one embedding model
  at two different dimensions across two different scopes — legal under the old per-`(model, scope)`
  dimension rule — now fails migration outright with `TopoError::Rejected`, naming the model and both
  dimensions, rather than silently picking one. Back up the `.redb` file first if you may need to roll
  back.

#### Changed

- **Embedding dimension is now pinned per model, permanently — not per `(model, scope)`.** Previously
  the same model name could carry different vector dimensions in different scopes. As of this version,
  a model's first `SetEmbedding` anywhere pins its dimension for good: a later `SetEmbedding` under the
  same model with a different dimension, in ANY scope, rejects the whole batch. **If you embedded the
  same model name at different dimensions in different scopes, those writes will start failing** —
  rename one of the models. A zero-dimension embedding is still rejected up front, before it can pin
  anything.
- The in-RAM per-`(model, scope)` vector index ("the slab") and its locking machinery are removed —
  internal only, no public API change. `search_vector` now reads the on-disk `vectors`/`embedding_ref`
  tables directly, so there is no in-memory index to warm, poison, or rebuild on open.

#### Fixed

- **Full-text posting maintenance was quadratic in corpus size.** Every touch to a term's posting list
  used to rewrite that term's ENTIRE row (read-decode-insert-encode-write the whole thing), so indexing
  cost per document grew with how much of the corpus already shared that document's vocabulary — a
  250k-memory build projected to hours (see [BENCHMARKS.md](BENCHMARKS.md)'s "FTS posting maintenance
  is quadratic" finding). Postings are now split into ~8 KiB chunks; a new document's posting update
  touches, and decodes, exactly one chunk regardless of how large the term's posting list has grown.
  Before/after throughput numbers: **(numbers: Task 9)**.

### 0.0.6

#### Breaking

- **On-disk format v3** ([FORMAT.md](FORMAT.md)): dense slot keys for nodes/edges (ULIDs are no
  longer the record key), interned scopes (`SCOPES` registry, `ScopeId` -> small integer id),
  chunked adjacency (`OUT_ADJ`/`IN_ADJ` replace full-scan edge lookup), an on-disk equality index
  (`PROP_INDEX`, no longer rebuilt in RAM at every open), and a re-keyed FTS layout (postings/doc
  stats keyed by scope id + dense slot instead of ULID).
- **Public `Snapshot`/`AdjEntry` types removed.** The in-memory snapshot layer they belonged to is
  gone; reads now run directly against redb MVCC read transactions instead of a materialized
  snapshot copy.

#### Added

- **ONE-WAY auto-migration of v1/v2 files on open.** An existing v1 or v2 database file is
  migrated to v3 automatically the first time it's opened with this version — there is no path
  back to v1/v2. Migration re-keys `NODES`/`EDGES`/`EMBEDDINGS`/`COUNTERS` to dense slots, rebuilds
  the FTS tables in the v3 layout, and builds the v3 sidecar tables (slot maps, adjacency, scope
  registry, prop index) from the migrated rows.
- **`DbOptions { cache_size_bytes }`** and **`Db::open_with_options`**, threading redb's
  `Builder::set_cache_size` through to the underlying database.

#### Changed

- Corruption that previously surfaced as silent absence now surfaces loudly: a slot mapping
  (`NODE_SLOTS`/`EDGE_SLOTS`) with no matching record row is `TopoError::Encoding`, not
  `Ok(None)`/`Rejected`.
- Benchmarks are now recorded in [BENCHMARKS.md](BENCHMARKS.md), including the v3 size/throughput
  gates.

### 0.0.5

#### Changed

- **An edge scoped to a project unrelated to its endpoints is now rejected.** If either endpoint is
  project-scoped `A`, the edge must be scoped `A` or `shared`. Submitting one that isn't now returns
  `TopoError::Rejected` instead of committing.

  Such an edge had **inverted visibility**: it was invisible to the project that wrote it and visible
  to an unrelated project. A relationship asserted by project P leaked into project Q's reads and
  vanished from P's own. (It was *not* unreachable, as previously documented — the read path's scope
  gates are independent, so a reader spanning both projects saw it fine.)

  **A project-scoped edge between two `shared` nodes remains legal**, and is unaffected. It means
  "in project P, these two shared entities are related" — visible to P's reader, hidden from other
  projects — and is the reason a per-project scope is layered over a shared one at all.

  **Migration:** none for a database. Existing databases open unchanged, and an old op log containing
  such an edge still replays — the rule is enforced on submit, not on replay, so nothing already
  committed is retroactively condemned. A *client* that was silently creating these edges will now
  get an error; pass an explicit scope (`link`'s `scope` param on `topodb-mcp`, `--scope` on
  `topodb-cli`) to say what was meant.

### 0.0.4

> **Read this if you depend on `topodb` 0.0.3 from crates.io.** The 0.0.3 *published* to crates.io
> does **not** match the 0.0.3 in this repository's git history: fixes landed under the
> already-published version number and were never released. crates.io's 0.0.3 is therefore missing
> everything below. A published version is immutable and cannot be corrected in place, so 0.0.4 is
> the first release that carries these. Treat crates.io 0.0.3 as superseded.

#### Fixed

- **A zero-dimension embedding permanently poisoned a `(model, scope)` vector slab.**
  `SetEmbedding` with an empty vector was accepted, which fixed that slab's dimension at 0 — after
  which **every** real embedding under that `(model, scope)` was rejected as a dim conflict, with no
  way to recover. The op is now rejected up front (`TopoError::Rejected`), symmetric with
  `search_vector`, which already refused an empty query vector.

#### Changed

- `TopoError::Rejected`'s message is now `"rejected: {0}"` (was `"batch rejected: {0}"`). It is
  raised by read paths too — e.g. querying a prop that isn't equality-indexed — so the old wording
  was misleading. **If you string-match on that prefix, update it.**

---

## `topodb-json`

### 0.0.3

#### Added

- **`create_node` batch command** — creates nodes with arbitrary labels for host-defined schemas
  (the episode-recorder's `Episode`/`PolicyVersion` nodes are the first consumer). Reserved labels
  (`Memory`, `Entity`) are rejected — use `create_memory`/`create_entity` for those.

#### Changed

- Engine dependency moved to `topodb` 0.0.6 (on-disk **format v3**). See the engine's 0.0.6 entry —
  in particular the **one-way auto-migration** of existing database files on first open.

### 0.0.2

> **Read this if you depend on `topodb-json` 0.0.1 from crates.io.** As with the engine, the
> *published* 0.0.1 does not match this repository's 0.0.1 — it predates the entire batch DSL and
> the scope helpers, and `batch.rs` does not exist in it at all. 0.0.2 is the first release that
> carries them. Treat crates.io 0.0.1 as superseded.

#### Added

- The **batch DSL** (`resolve_batch`, `batch.rs`) — resolves a JSON command array into engine ops,
  with `#N` back-references to ids produced by earlier commands. Backs `topodb-cli submit` and the
  `submit_batch` MCP tool. Carries a per-op `scope` on `create_memory`, `create_entity`, and `link`.
- Scope helpers shared by both front ends, so the CLI and the MCP server cannot drift:
  `resolve_scope`, `scope_to_scope_set`, `scopes_to_scope_set`, `scope_label`.
- Single-sourced index-spec and label/prop constants (`default_spec`, `MEMORY_LABEL`,
  `MEMORY_CONTENT_PROP`, `ENTITY_LABEL`, `ENTITY_NAME_PROP`), so a CLI-created database and an
  MCP-created one carry a byte-identical persisted `index_spec`.

---

## `topodb-mcp`

### 0.0.5

> **Opening a database with this version migrates it, one-way.** This release embeds `topodb`
> 0.0.6, whose on-disk format is v3. The first time this server opens an existing v1/v2 database
> file it is auto-migrated to v3, and older builds can no longer read it. Back up the `.redb` file
> first if you may need to roll back.

#### Changed

- Embeds `topodb` 0.0.6 (format v3) and `topodb-json` 0.0.3.
- **Engine storage/encoding failures on `find_by_prop` and `traverse` are now reported as
  `internal_error`, not `invalid_params`.** These paths read from disk in v3 and can genuinely fail
  for reasons that are not the caller's; only `Rejected` (caller-fixable) maps to `invalid_params`,
  matching `search_memories`' existing contract. **If a client special-cases `invalid_params` from
  these two tools, note the narrowed meaning.**

### 0.0.4

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

### 0.0.3

> **Opening a database with this version migrates it, one-way.** This release embeds `topodb`
> 0.0.6, whose on-disk format is v3. The first `topodb` command against an existing v1/v2 database
> file auto-migrates it to v3, and older builds can no longer read it. Back up the `.redb` file
> first if you may need to roll back.

#### Changed

- Embeds `topodb` 0.0.6 (format v3) and `topodb-json` 0.0.3. No CLI surface changes.

### 0.0.2

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
