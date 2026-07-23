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

#### Added

- **`TopoError::Busy`**: lock contention on ANY open path (`open`, `open_with`, `open_stored`,
  including the persisted-spec read) is a typed, retryable variant instead of an opaque storage
  error. Enables graceful degradation and callers to implement retry loops (e.g. with
  exponential backoff). A caller invoking `Db::open*` under concurrent contention now receives
  `Busy` instead of storage-layer errors; the engine returns this variant immediately without
  blocking, so the caller owns the retry policy.

### 0.0.10 — 2026-07-22

#### Added

- **`Db::nodes_by_label_unbumped`** — a label scan that returns the same population
  and order as `nodes_by_label` but does NOT bump the access counters. For
  maintenance sweeps that read the population to inspect it (e.g. staleness reads
  `last_accessed_at`, and bumping it would erase the very signal) rather than to
  recall it — a read for housekeeping is not a recall.
- **`RecallQuery.tombstone_prop`** (`Option<String>`, default `None`) — a
  post-fusion filter that drops any candidate whose named `Int` prop is `<=` the
  effective now (`options.now_ms` when set, else wall clock). Powers supersession:
  a memory marked `superseded_at` disappears from recall as of its supersession,
  while an `as_of`-past query still sees it. **Breaking for struct-literal
  construction** of `RecallQuery` (new field) — same caveat as the recall-tuning
  fields; use `RecallQuery::new(..)` and set fields.

### 0.0.9 — 2026-07-20

#### Added

- **`suggest_links` similarity transparency** — each `LinkSuggestion` now carries `similarity`
  (the semantic leg's raw cosine; `None` when the suggestion is structural-only), and
  `SuggestLinksQuery.min_semantic_similarity` optionally floors the semantic leg (validated
  `-1.0..=1.0`, `None` default = prior behavior byte-for-byte). RRF rank scores hid the
  strong-vs-weak distinction; the raw cosine restores it. **Breaking for struct-literal
  construction** of `SuggestLinksQuery` (new field), same caveat as the `RecallQuery` entry.
- **Recall tuning on `RecallQuery`** — `labels` (post-fusion allowlist, `None` = unfiltered),
  per-leg RRF weights (`text_weight`/`vector_weight`/`graph_weight`, defaults 1.0/1.0/0.5 — the
  former compile-time constants), and `access_weight` (0-1, default 0 = off): an opt-in
  post-fusion boost `1 + w·ln(1+count)/(1+ln(1+count))` from the access counters — neutral at
  count 0, log-damped, read without bumping. Recency and access apply in one combined
  post-fusion pass. Defaults are byte-identical to the previous behavior (MRR golden-set gate
  unchanged). Zero-weight legs now contribute nothing to fusion (previously a zero-weight leg still
  injected its candidates at score 0 — a pre-existing ghost-entry bug fixed in this change); a
  skipped zero-weight leg also no longer bumps access counters for hits it would have returned.
  **Breaking for struct-literal construction:** `RecallQuery` gained fields — use
  `RecallQuery::new(scopes, query, k)` with struct-update syntax so future additions don't
  break your call sites.
- **Write path: intern journal + group commit.** Batches no longer reload the dictionary/scope
  registry from disk (aborted batches revert exactly their own interns); the in-memory mirrors'
  write guards release before the commit fsync, so readers never block on an in-flight writer;
  queued submits coalesce into one transaction (≤16 batches/4096 ops) with per-submit atomicity
  preserved via individual replay on group failure. `ensure_index_spec` no longer commits on
  no-op opens. Crash note: a crash during a group commit loses the whole group — coarser, never
  finer, than before; no caller was ever promised its batch survived a crash that preceded its
  reply.
- **⚠️ ON-DISK FORMAT v6.** New `label_index` table (`(label, scope, ulid) → slot`, derived
  state). **Every existing database migrates irreversibly on first open after this upgrade**
  (one full node scan; see FORMAT.md §v6). `nodes_by_label` now loads only matching rows; new
  `nodes_by_label_newest(scopes, label, k)` serves newest-first reads near-O(k) (session-start
  injection's `recent_memories` uses it); float-range scans stop decoding embeddings. Unselective
  full-label scans stay flat; selective-label scans run ~250x faster and newest-first k-bounded
  reads ~1,400x faster on a 10k corpus (same-machine criterion numbers). `recent_memories` (and any
  other newest-k read going through `nodes_by_label_newest`) now bumps access counters only for the
  `k` nodes actually returned, rather than for every node of the label — a deliberate narrowing.
- **`SetEmbedding` rejects non-finite components** (NaN/±Inf corrupt cosine scoring).
- **`Db::suggest_links` — per-node link prediction.** Ranks the k likeliest missing edges from a
  node: RRF fusion of a structural leg (PPR over the 3-hop neighborhood, self and live 1-hop
  neighbors excluded) and a semantic leg (cosine against the node's own stored embedding), with
  shared-neighbor evidence per suggestion. Read-only — edge creation and typing stay host policy.
- New tests: kill-during-commit crash recovery (25-round SIGKILL harness), read-during-write
  latency, group-commit semantics, differential-oracle coverage for the label index.

#### Changed

- **Recall graph leg ranks by Personalized PageRank** — one 1-hop `Both` traversal from the top
  `GRAPH_SEEDS` preliminary seeds together (was one traversal per seed), scored by deterministic
  bounded power iteration with teleport weighted by preliminary fused score. Connectivity now
  orders the leg — a node several seeds converge on outranks a node dangling off one — replacing
  flat seed-rank concatenation. Membership stays 1-hop: the golden-set eval rejected 2-hop reach
  (entity-fan-out hubs crowded correct hits out of top-3). Same `graph_boost` flag, half weight,
  and determinism contract; eval green. No format change (v6).

### 0.0.8 — 2026-07-18

#### Added

- **`Db::recall` + `RecallQuery`** — hybrid recall over up to three legs, fused with Reciprocal
  Rank Fusion (`RRF_K = 60`): a BM25 **text** leg (`search_text_expanded`, honoring host-supplied
  synonym expansions); a cosine **vector** leg when the query carries a `(model, vector)` pair
  (omitted, not erroring, if the model has no vectors); and a two-stage **graph** leg — the
  preliminary text+vector fusion's top 5 hits become seeds, their 1-hop neighbors (both
  directions) are pulled in at half weight (`WEIGHT_GRAPH = 0.5` against `WEIGHT_TEXT`/
  `WEIGHT_VECTOR = 1.0`) — toggled by `graph_boost`. Recency weighting is deliberately applied
  **once, after fusion** (each leg runs with `recency_weight: 0.0`), so freshness can't be
  double-counted across legs. `search_text_expanded` and the public `topodb::analyze` (the same
  camelCase-split/lowercase/Snowball-stem pipeline FTS already used internally) are now exported
  so callers can pre-analyze synonym terms consistently with stored content.
- **Golden-set recall-quality gate** (`crates/topodb/tests/recall_quality.rs`): a fixed ~62-memory/
  ~18-entity corpus with hand-labeled expected top hits, scored by Mean Reciprocal Rank across four
  configs (bm25-only, +vector, +graph, full hybrid). Measured at landing: bm25-only **0.718** →
  +vector **0.748** → full hybrid **0.760**; the full-hybrid config additionally asserts every
  query's expected id lands in the top 3. The suite hard-gates on `MRR_FLOOR = 0.740` (measured
  minus a 0.02 margin) for the full-hybrid config, so a regression that erodes recall quality fails
  CI instead of silently degrading behind a fusion change.
- **Normalized equality lookup** (`Db::nodes_by_prop_normalized`): case- and whitespace-insensitive
  matching for `Str` values — the dedup primitive that lets a caller resolve "drew powell" to a
  stored "Drew Powell" instead of minting a duplicate. `nodes_by_prop` keeps byte-exact semantics
  via a record-level post-filter.
- **`Db::edges_from`** — scoped listing of a node's outgoing edges, filterable by target, edge type,
  and open-only. The supersession primitive: find the open edges a changed fact should close,
  without a full traverse.
- **Recency-weighted text search** (`Db::search_text_with` + `SearchOptions`): each hit's BM25 score
  is multiplied by `(1-w) + w·2^(-age/half_life)`, with age read from the node id's ULID timestamp
  (also newly exposed as `NodeId::timestamp_ms` etc.). Opt-in; `search_text` is unchanged
  (weight 0). Applied before top-k truncation, so fresh hits can displace stale ones out of the
  window, and floored so a strong old match is never erased.
- **Stemming analyzer (v1)**: FTS tokenization is now split-on-non-alphanumeric → camelCase split
  (acronym-aware: `parseHttpRequest` → `parse`/`http`/`request`, `HTTPServer` → `http`/`server`) →
  Unicode lowercase → Snowball English stem (via the pure-Rust `rust-stemmers` dep), applied
  identically to documents and queries — `databases` matches `database`, `running` matches `run`.
  The pipeline is versioned in META (`"fts_analyzer_version"`); a file built under a different (or
  pre-stamp) analyzer gets its FTS tables drained and rebuilt on open, same machinery as the
  PROP_INDEX norm stamp.
- **Miss-only fuzzy/prefix fallback** (`SearchOptions::fuzzy_fallback`, default ON): a query term
  with zero df in a scope — it would contribute nothing anyway — expands to its closest vocabulary
  neighbors (prefix matches ≥3 chars, bounded edit distance ≤1 for 3-5-char terms / ≤2 for longer),
  capped at 4 candidates whose BM25 contributions are discounted 0.6×, so exact hits always
  dominate and hitting queries pay nothing. Query-time only: the scope vocabulary is enumerated
  from the existing scope-prefixed postings keys — no auxiliary index, no format change,
  deterministic.

#### Changed

- **Format v5** (`FORMAT_VERSION = 5`): PROP_INDEX `Str` keys are now stored under their normalized
  form (`prop_index::normalize_str`), and FTS postings under the v1 stemming analyzer; no table
  layout changed. Existing files upgrade on first open — the v4→v5 arm stamps the version and
  `ensure_index_spec` drains + rebuilds both indexes, driven by the new `"prop_index_norm_version"`
  and `"fts_analyzer_version"` META stamps (pre-v5 files lack both). Pre-v5 builds refuse a v5 file
  with `UnsupportedFormat` rather than silently missing every `Str` probe. See FORMAT.md.

#### Fixed

- **Edit-heavy re-indexing no longer grows a covering postings chunk without bound.** Adding a term
  to many OLD (low-slot) documents — bulk retroactive tagging — routed every insert into one covering
  chunk that never split, growing per-edit cost 2.8× over 12k edits (BENCHMARKS.md Gate 6b). Covering
  chunks now split at the same 4 KiB target as the append path (a mid-list split renumbers the chunks
  behind it; raw bytes move untouched), and the covering chunk is found by binary-searching first
  slots peeked from chunk headers instead of decoding chunks front-to-back. Gate 6b is now a hard
  gate (≤ 1.5× growth 1k→12k edits) asserted inside the benchmark itself. **No format change** — v4
  files need no migration; this is maintenance behavior only.

### 0.0.7

#### Breaking

- **On-disk format v4** ([FORMAT.md](FORMAT.md)): clustered vector storage — `vectors`/`embedding_ref`/
  `vector_dims` replace the old slot-keyed `embeddings` cold table — and a chunked full-text postings
  layout (`postings` re-keyed from one row per term to `[scope][term][chunk]`, ~4 KiB per chunk). See
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
  tables directly, so there is no in-memory index to warm, poison, or rebuild on open. **User-visible
  payoff:** opening a 1M-memory database with 20% embeddings went from a p95 of ~2.1 s (v3, rebuilding
  the RAM slab from `EMBEDDINGS` on every open) to a p95 of ~11 ms (v4, ~186× faster) — see
  [BENCHMARKS.md](BENCHMARKS.md)'s gate 1.

#### Fixed

- **Full-text posting maintenance was quadratic in corpus size.** Every touch to a term's posting list
  used to rewrite that term's ENTIRE row (read-decode-insert-encode-write the whole thing), so indexing
  cost per document grew with how much of the corpus already shared that document's vocabulary — a
  250k-memory build projected to hours (see [BENCHMARKS.md](BENCHMARKS.md)'s "FTS posting maintenance
  is quadratic" finding). Postings are now split into ~4 KiB chunks; a new document's posting update
  touches, and decodes, exactly one chunk regardless of how large the term's posting list has grown.
  Before/after throughput numbers (Task 9, full spec — entities, edges, and text — same synthetic
  agent-memory workload as the rest of [BENCHMARKS.md](BENCHMARKS.md)): **before (v3), measured**:
  ~37 ms/doc and climbing at a 75k-doc corpus (a 250k build projected to ~3.8 h and never completed).
  **After (v4), measured**: ~0.66 ms/doc at 10k docs, ~1.10 ms/doc at 100k docs (1.66× the 10k figure,
  not the unbounded climb v3 showed) — a 100k-doc full-spec build, with the text index enabled, now
  completes in **106 s** instead of hours. `POSTINGS_CHUNK_TARGET` was also re-tuned from 8 KiB to
  4 KiB based on this task's chunk-size experiment (4 KiB won on both indexing and edit cost, and tied
  for best on search latency, at a 10k-doc corpus). One caveat carried forward, not fixed here: a
  document repeatedly edited to *gain* a term whose covering posting chunk isn't that term's last
  chunk can still grow that chunk without a split (an accepted, scoped simplification — splitting was
  only in scope for the append path); see BENCHMARKS.md's "Gate 6b" finding for the measured curve.

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

### Unreleased

#### Added

- **`compose` module** — the shared remember verb planning layer: `plan_remember` (exact-content
  dedup, alias-aware find-or-create entities, idempotent links, supersession) plus the lookup
  helpers (`find_existing_entity`, `existing_memory`, `content_hash`) and `RememberRequest::validate()`
  moved from `topodb-mcp` so both the MCP server and the CLI can delegate composition logic to the
  same place. Also `open_with_busy_retry` — a helper that retries any caller-supplied open closure
  on `TopoError::Busy` with configurable budget and exponential backoff.

### 0.0.7 — 2026-07-22

#### Added

- **`MEMORY_CONTENT_HASH_PROP`** (`"content_hash"`) plus a `Memory`/`content_hash`
  equality index in `default_spec()` — the dedup primitive that lets the write
  front ends resolve a re-stored fact to the memory that already holds it, instead
  of accumulating identical copies.
- **`MEMORY_SUPERSEDED_AT_PROP`** (`"superseded_at"`) — the supersession tombstone
  marker (a millisecond timestamp) that recall filters on via
  `RecallQuery.tombstone_prop`.

### 0.0.6 — 2026-07-20

- Dependency pin only: `topodb` 0.0.9. No functional change.

### 0.0.5 — 2026-07-18

#### Added

- **`Alias` and `Synonym` label/prop constants**, alongside the existing `Entity`/`Memory` ones —
  the shared vocabulary `topodb-mcp`'s `add_alias`/`add_synonym` tools and their index-spec entries
  are built from, so the two crates cannot drift on what an alias or synonym node looks like.
- **`normalize_edge_type`** — the shared edge-type vocabulary normalizer (lowercase; whitespace/
  hyphen/underscore runs collapse to a single `_`), used by the MCP `link` tool, the batch DSL's
  `link` command, and `topodb-cli link`, so the three write paths can no longer fragment the edge
  type dictionary (`works_at` vs `Works At` vs `works-at`).
- **`upgraded_spec`** — maps a db's persisted spec forward when (and only when) it is exactly a
  stock default this crate has shipped; customized specs are returned unchanged. Used by
  `topodb-mcp` and `topodb-cli` to roll the default-spec change below out to existing stock dbs.

#### Changed

- **`default_spec` is now v3**: text-indexes `(Entity, name)` and `(Alias, name)` in addition to
  `(Memory, content)`, and equality-indexes `(Alias, name)` and `(Synonym, term)` in addition to
  `(Entity, name)` — so
  `search_memories`/`search-text` can find an entity or its aliases by name, and alias/synonym
  lookups have an index to run against, instead of relying solely on exact-match `find_by_prop`.
  `upgraded_spec` is now **generation-aware**: it recognizes a db on ANY older stock generation
  (not just the immediately-previous one) and maps it forward to v3 in one step, so a db that has
  never been `--spec`-customized picks up every generation's additions on its next open regardless
  of how many versions behind it is; customized specs are still returned unchanged. Batch `link`
  commands now normalize their `type` field.

### 0.0.4

#### Changed

- Engine dependency moved to `topodb` 0.0.7 (on-disk **format v4**). See the engine's 0.0.7 entry —
  in particular the **one-way auto-migration** of existing v1/v2/v3 database files on first open,
  and the per-model embedding-dimension rule. No `topodb-json` surface changes.

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

### Unreleased

#### Changed

- **Internal refactor**: `remember` and entity-lookup composition now delegates to the shared
  `topodb-json::compose` module instead of maintaining separate logic in the MCP server. The
  startup open path now retries on `TopoError::Busy` using the same `TOPODB_LOCK_WAIT_MS` env
  var. No tool-surface change.

### 0.0.12 — 2026-07-22

A full memory-hygiene layer for topodb-as-agent-memory: prevent redundancy on
write, detect what has accreted, and act on it — all advisory (nothing
auto-merges). Tool count 21 → 27.

#### Added — hygiene

- **Write-time dedup** — re-storing identical content via `remember`/`create_memory`
  resolves to the existing memory (`deduplicated: true`) and only links entities it
  did not already have, instead of stacking copies (content is FNV-hashed and
  equality-indexed). (#16)
- **Supersession** — `remember`'s `supersedes: [ids]` retires a memory when a fact
  changes: marks `superseded_at`, closes its open out-edges, and recall drops it as
  of now (still visible in `as_of`-past queries). (#17)
- **Semantic near-duplicate detection**, banded and contradiction-aware — write-time
  `near_duplicates` and the `find_duplicate_memories` scan surface semantically
  close memories with a `band` (`likely` cosine ≥ 0.80 / `possible` 0.68–0.80, the
  widened review net) and a `relation` (`duplicate` → merge / `supersession` → the
  pair CONTRADICTS, retire the stale side). A deterministic negation-cue check
  distinguishes contradictions from restatements — raw cosine can't, since it scores
  contradictions even higher than reworded duplicates. (#18, #20, #27, #28)
- **`consolidate_memories`** — merge a near-duplicate pair: keep one, inherit the
  other's unique relationships, supersede it, atomically. (#22)
- **`find_orphan_memories`** — live memories with no open outgoing edges (stored but
  linked to nothing, reachable only by search). (#23)
- **`find_stale_memories`** — memories cold beyond `older_than_days` (activity = the
  later of creation and last recall), stalest first; the scan is non-bumping so it
  never resets the recency signal it reads. (#24)
- **`memory_health`** — one call that runs all three scans and returns a summary:
  `duplicate_pairs` vs `supersession_pairs`, `orphan_count`, `stale_count`, a
  `needs_attention` flag, and sample rows. The session-start orientation read. (#25)
- **`traverse` multi-seed** — `seed_ids` starts a walk from several nodes at once
  (e.g. every `search_memories` hit) in a single call. (#15)

#### Changed

- The maintenance scans (`find_duplicate`/`find_orphan`/`find_stale`,
  `memory_health`) read via the engine's non-bumping label scan, so a housekeeping
  sweep never inflates the access-boost or resets the recency of everything it
  examines.

#### Fixed

- Deflaked `recent_memories` ordering: it sorts by ULID, and `Ulid::new()` is not
  monotonic, so same-millisecond creates could sort out of order. (#19)

### 0.0.11 — 2026-07-20

#### Added

- **`suggest_links` evidence & similarity (breaking shape change)** — `common_neighbors`
  entries are now `{id, label, name}` objects (name: the `name` prop, else `content`
  truncated to 80 chars) instead of bare ULID strings, saving a `get_node` round-trip per
  neighbor; each suggestion carries `similarity` (`null` = structural-only); new optional
  `min_similarity` param floors the semantic signal. Breaking only for the days-old
  `suggest_links` tool shape at 0.x.
- **`remember`** — a composed, atomic storage verb: one call creates the memory, find-or-creates
  each named entity (`create_entity` semantics: case/whitespace-insensitive across read scopes +
  write scope + shared, alias-aware, oldest-id-wins; repeated names within one call collapse
  first-spelling-wins), and links memory→entity (`about` by default, `edge_type` to override) —
  all in a **single engine batch**, so a stored fact can never strand unlinked. Params:
  `content`, `entities` (non-empty), `edge_type?`, `props?`, `scope?` (one scope for everything
  the call creates). Tool count 19 → 20.
- **`recent_memories`** — newest-first orientation read (`k` ≤ 100, default 8), the no-query
  recency read session-start injection needs. Tool count 20 → 21. Full `nodes_by_label` scan,
  documented as acceptable pending a label index.
- **ONNX Runtime auto-download** — on first run with embeddings enabled and no system runtime,
  the server fetches the official Microsoft ONNX Runtime build for the platform (pinned to
  **1.24.2**, the version ort-sys 2.0.0-rc.12 distributes; archive sha256 verified against
  compiled-in pins BEFORE extraction) into `<model-dir>/ort/1.24.2/`, atomically and
  concurrent-start-safe. Resolution precedence: `ORT_DYLIB_PATH` (exclusive) → system runtime →
  cached download → fetch. New flag `--no-ort-download` disables fetching; every failure still
  degrades to text+graph-only exactly as before. Closes the install cliff where
  `cargo install topodb-mcp` / npm users silently never got vector recall. macOS coverage is
  arm64-only — Microsoft publishes no Intel-Mac 1.24.2 artifact, so Intel Macs keep the manual
  path (system runtime or ORT_DYLIB_PATH).
- **`search_memories` tuning params** — `labels` (result label allowlist, **new default
  `["Memory","Entity"]`**: every label outside Memory/Entity — Alias/Synonym plumbing nodes and
  any custom host labels alike — no longer surfaces in default results — a behavior change;
  override to widen or narrow), `text_weight`/`vector_weight`/`graph_weight`
  (0-10, defaults 1/1/0.5), and `access_weight` (0-1, default 0): opt-in boost for
  frequently-recalled memories.
- **`suggest_links` tool** — surfaces the engine's link predictions (score, structural/semantic
  flags, common-neighbor evidence) under the active embedder's model namespace. Suggestions only:
  the agent reviews and `link`s the ones it agrees with.

#### Changed

- Tool descriptions repositioned around `remember` as the primary storage verb:
  `create_memory` (unlinked note), `create_entity` (props-carrying upsert), and `link`
  (entity↔entity relations, supersede) are now described as its building blocks; `get_info`
  instructions updated to match.

#### Release checklist

- Publish to npm, then **bump the Claude Code plugin's server pin**
  (`plugins/claude-code/server-args.js`'s `SERVER_VERSION`) to this version, re-verify
  `plugins/claude-code/test/broker.test.js` against the real published package, and re-point the
  plugin's `SKILL.md` + `/remember` command at the `remember` tool **in the same commit as the
  pin bump** — the plugin must never document tools its pinned server doesn't have.

### 0.0.10 — 2026-07-18

#### Fixed

- **Servers without an ONNX Runtime library became unkillable zombies holding the database lock.**
  `ort`'s load-dynamic FAILURE path re-enters its own `OnceLock` while constructing the load error
  (upstream bug), permanently deadlocking the embedder init thread — and ort's
  `release_env_on_exit` atexit handler then blocks `exit()` on the mutex that thread holds, so the
  process survives stdin EOF forever and every later open of the same db fails with
  `DatabaseAlreadyOpen` (caught by the plugin broker's idle-exit test on Linux CI). The embedder
  now pre-flights the dylib with `libloading` before any ort call: no loadable ONNX Runtime lands
  `failed` status cleanly (text+graph-only recall, per the degradation contract) and the process
  exits normally. Release checklist: bump the plugin's `SERVER_VERSION` pin to 0.0.10.

### 0.0.9 — 2026-07-18

#### Added

- **`get_edges` tool** (17 tools now): list a node's outgoing edges, filterable by target/type,
  open-only by default — how an agent finds the edge id to `close_edge`, and checks what a node is
  already linked to. Type filters match both the normalized and raw stored forms.
- **`link` gains `supersede: true`**: atomically closes every other open same-type edge from the
  source before creating/reusing the new one — the "changed employer/owner/team" flow — reporting
  the closed ids in `superseded`.
- **Recency-weighted `search_memories`** (`recency_weight`, default 0.3; `recency_half_life_days`,
  default 30): fresher memories outrank stale ones at equal BM25 relevance; `recency_weight: 0`
  restores pure BM25.
- **`search_memories` stems and fuzzy-recovers**: query terms are analyzed like documents
  (camelCase split + Snowball stem), and a term matching nothing falls back to close prefix/typo
  neighbors at a score discount (`fuzzy: false` disables). Tool description and server
  instructions now say what search does and doesn't handle.
- **`add_alias` and `add_synonym` tools** (19 tools now): `add_alias(entity_id, alias)` registers an
  alternate name for an existing entity ("Drew" for "Drew Powell") — `create_entity`, `find_by_prop`,
  and `search_memories` all resolve it to the canonical node from then on; errors if the alias
  already names a different entity (a merge situation, both ids reported). `add_synonym(term,
  expansion, bidirectional = true)` teaches search a domain equivalence ("auth" ↔ "login") — terms
  and expansions are stored/looked up in analyzed (stemmed) form so `add_synonym('auth','login')`
  also catches `"logins"`, expansion is depth-1 only (synonyms never chain), and query-time
  resolution is capped at 4 expansions per term (sorted, deduped, truncated). Both are ordinary
  nodes — `remove_node` retires either.
- **Local embeddings subsystem**: `--embeddings <off|model>` (default: auto-loads
  `bge-small-en-v1.5`, 384-dim) and `--model-dir <path>` (default `~/.cache/topodb/models`) flags.
  Write-path embedding happens automatically and atomically (`create_memory`/`create_entity` fold
  a `SetEmbedding` op into the same batch as the `CreateNode`) once the embedder reaches `ready`;
  a startup backfill embeds any node created while the embedder was still loading, driven by
  replaying `ops_since` rather than a per-scope label scan (matches the change-feed doctrine, needs
  no new engine API). `db_info` reports `embeddings: { model, status }` (`off`/`downloading`/
  `ready`/`failed`) so a client can tell whether the vector leg is live. **Requires an ONNX Runtime
  dynamic library on the host** — this server is built against fastembed's `ort-load-dynamic`, so
  embeddings only reach `ready` once a compatible ONNX Runtime dylib is discoverable (e.g.
  `brew install onnxruntime`; the loader honors `ORT_DYLIB_PATH`, e.g.
  `/usr/local/lib/libonnxruntime.dylib`). Without one, status is `failed` and the server runs
  exactly as before — text+graph-only recall, no write-path embedding, no other change in
  behavior.

#### Changed

- **`search_memories` now runs hybrid recall** (`Db::recall`) instead of plain BM25: a `graph_boost`
  param (default `true`) adds a two-stage graph leg — the preliminary text+vector fusion's top 5
  hits become seeds, their 1-hop neighbors are pulled in at half weight — RRF-fused (k=60) with the
  text and, when the embedder is `ready`, vector legs; recency weighting moved to apply once, after
  fusion, rather than inside the text leg alone. Learned synonyms (`add_synonym`) now expand a
  query's terms automatically. None of this is a breaking param change — every existing call
  without `graph_boost` still gets it (default on).
- **`create_entity` is now find-or-create**, and alias-aware. The name is matched case- and
  whitespace-insensitively across the read scopes, the write scope, AND `shared`, and — via
  registered aliases (`add_alias`) — resolves an alternate name to its canonical entity too; an
  existing entity is returned with `created: false` (oldest wins among pre-existing duplicates, so
  links converge) and new props keys are merged without overwriting. This closes the main
  duplicate-entity path: an unconditional create guarded only by advisory "check first" prose.
- **`find_by_prop` also resolves aliases** for `(Entity, name)` lookups with `exact: false` — an
  alias name now returns the canonical entity it points to, not a miss. `exact: true` and every
  other `(label, prop)` pair are unaffected.
- **`link` is idempotent per `(from, to, type)`** within the write scope — an identical open edge
  is reused (`created: false`) instead of stacking a parallel duplicate — and **edge types are
  normalized** (`Works At` == `works-at` == `works_at`). `traverse`'s `edge_types` filter probes
  raw and normalized forms.
- **`find_by_prop` matches strings case/whitespace-insensitively by default**; pass `exact: true`
  for the old byte-exact behavior.
- **Temporal-bound sanity guards**: `link.valid_from` / `close_edge.valid_to` reject
  seconds-since-epoch values (would date the edge to January 1970) and future timestamps (would
  make the edge invisible to every "now" read) with actionable errors.
- **Stock-spec auto-upgrade on open**: a db still on an older stock default spec (never
  `--spec`-customized) is upgraded to the current default — adding the `(Entity, name)` text index
  so entities are searchable by name — with a one-time reindex. Customized specs are untouched.
- Tool descriptions and server instructions rewritten around the new semantics: always link what
  you store, supersede when a to-one fact changes, retry token-variant queries before concluding
  nothing is stored.

#### Release checklist

- **Bump the Claude Code plugin's server pin** (`plugins/claude-code/server-args.js`'s
  `SERVER_VERSION`, currently still `"0.0.8"`) to this version once it is published to npm, and
  re-verify `plugins/claude-code/test/broker.test.js` against the real published package — see
  `plugins/claude-code/README.md`'s "Server version" section for why the pin can't move early.

### 0.0.8

No engine or tool-surface changes. This release exists to ship a fix in the **npm launcher**
(`@topodb/topodb-mcp`'s `bin/topodb-mcp.js`), which is what selects and executes the platform binary.

#### Fixed

- **The launcher could execute a `topodb-mcp` binary belonging to a different install — silently.**
  It located the platform binary with a bare `require.resolve`, and Node's resolution **walks up the
  directory tree**. On a Windows host where npm had installed the wrong platform's optional
  dependency (`topodb-mcp-linux-x64` on win32), `topodb-mcp-win32-x64` was absent from the install —
  so the walk-up continued past it, found a stale `topodb-mcp-win32-x64@0.0.3` elsewhere on the
  machine, and resolved *successfully*. Because it succeeded, the launcher's "prebuilt binary package
  is not installed" error — whose entire purpose is that situation — never fired, and a server two
  on-disk-format generations old was launched while every version check in the stack reported 0.0.7.

  A successful resolve is not proof the binary is ours. `optionalDependencies` pins each platform
  package to the launcher's exact version, so the launcher now **verifies the resolved package
  reports that version** and refuses otherwise, naming both the version it found and the path it came
  from. A wrong binary is now a loud, actionable error instead of a working-looking server with the
  wrong on-disk format.

### 0.0.7

#### Added

- **Per-request scope overrides via JSON-RPC `_meta`.** A request may now carry `topodb/scope` (a
  `"shared"`/ULID string) and/or `topodb/read_scopes` (a non-empty array of them) in its `_meta`
  envelope; they override `--scope` and `--read-scopes` **for that request only**. An explicit
  `scope`/`scopes` *argument* still wins over both, so nothing about the existing tool surface
  changes — this replaces the fallback, it does not pin the request. A client that sends no `_meta`
  is byte-for-byte unaffected.

  This exists because `--scope`/`--read-scopes` are *process-wide*, and that assumption breaks the
  moment one server process is shared by several clients. redb permits only one process to hold a
  database, so the Claude Code plugin's broker multiplexes every concurrent session onto a single
  `topodb-mcp` — and sessions in different projects need different scopes. Scope has to travel with
  the request, not the process.

  Passing `topodb/scope` **without** `topodb/read_scopes` narrows the read set to that one scope,
  mirroring how `--read-scopes` defaults to `--scope` when omitted. Inheriting the process-wide read
  set there would reintroduce exactly the leak this closes.

#### Fixed

- **Cross-project memory leak in the Claude Code plugin (`plugins/claude-code`).** Every project
  after the first silently read *and wrote* into the first project's scope: the broker is keyed on
  the database path alone, which is identical for all projects, so whichever session spawned it
  fixed `--scope` for every session that connected afterwards. A project's agent could recall
  another project's private memories. Requires the plugin at `SERVER_VERSION` 0.0.7, which now sends
  each session's scopes per request. Regression tests:
  `plugins/claude-code/test/broker.test.js` — `each_session_writes_to_its_own_project_scope` and
  `one_project_cannot_read_another_projects_memory`.

### 0.0.6

> **Opening a database with this version migrates it, one-way.** This release embeds `topodb`
> 0.0.7, whose on-disk format is v4. The first time this server opens an existing v1/v2/v3
> database file it is auto-migrated to v4, and older builds can no longer read it. Back up the
> `.redb` file first if you may need to roll back. Additionally: a v3 file holding one embedding
> model at two different dimensions across scopes (legal under v3's rules) **fails migration**
> with an error naming the model — re-embed under distinct model names before upgrading.

#### Changed

- Embeds `topodb` 0.0.7 (format v4) and `topodb-json` 0.0.4. Vector search now reads clustered
  on-disk tables (no in-RAM index to rebuild at open — a 1M-memory database with embeddings opens
  in ~11 ms instead of ~2.1 s), and full-text indexing cost is flat per document instead of
  growing with corpus size. No MCP tool-surface changes.

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

## Claude Code plugin

### Unreleased

#### Added

- **Hooks: session-start memory injection + observational episode capture.** SessionStart injects
  up to 8 recent, access-ranked project memories (hard char cap, 2.5s deadline, main sessions
  only, `startup`/`clear` sources only). PostToolUse records what each retrieval tool returned
  into a session state file; SessionEnd judges which memories the transcript actually used and
  writes the pi-vocabulary `Episode`/`RetrievalEvent` graph through the broker. Capture defaults
  on; `TOPODB_RECORDING=0` disables. Hooks never spawn the broker and always exit 0 — every
  failure degrades to exactly the pre-hook behavior. NOTE: injection requires a server with
  `recent_memories` (0.0.11) — with the currently pinned 0.0.10 it silently degrades to no
  injection; capture works against 0.0.10. Ships fully with the 0.0.11 pin bump. At that pin bump,
  also remove SKILL.md's "presently disabled" phrasing and capture a real PostToolUse payload
  fixture (`TOPODB_HOOK_DEBUG=1`) to confirm the normalizer's branch against production shape.
- **Session-start memory-health nudge.** The session-start hook also runs `memory_health`
  concurrently with the recall injection and, when the store has accreted cruft, appends a
  one-line advisory nudge (`🧹 Memory hygiene: N duplicate pairs, N supersessions, N orphans,
  N stale …`). Concurrent, timeout-guarded, and swallowed on any error, so it never delays or
  risks the memory injection; a server without `memory_health` yields no nudge. Requires the
  0.0.12 pin. (#26)

---

## `topodb-cli`

### Unreleased

#### Added

- **`remember` subcommand** — atomic store-and-link-entities in one call (`--content`, repeatable
  `--entity` [required ≥1], `--edge-type` default `"about"`, repeatable `--supersedes`, `--props`,
  `--scope`). Output: `{"memory_id","deduplicated","entities":[{"name","id","created"}],"edge_ids","superseded"}`.
  Combines memory creation, entity find-or-create, and linking into a single engine batch, so stored
  facts never strand unlinked.
- **`--lock-wait-ms`** / **`TOPODB_LOCK_WAIT_MS` env var** (default 3000, `0` = fail fast) — global
  flag for all subcommands; configures how long to retry on `TopoError::Busy` at startup. Lock
  exhaustion reports `{"error":{"kind":"busy",...}}` and exits with code 3.

#### Changed

- **Breaking: `create-entity` is now find-or-create by default** — the name is matched case- and
  whitespace-insensitively across write scope and shared, and resolves aliases.
  Existing entity is returned with `"created": false`; `--always-create` opts out (raw create,
  old behavior). Both paths now report the `created` flag in their output. When `created: false`,
  `--props` merges only NEW keys; a `name` key in props is always rejected.
- **Breaking: `create-memory` now stamps `content_hash` and reports `deduplicated`** — identical
  content (after whitespace normalization) resolves to the existing memory; `"deduplicated": true`
  indicates a hit, `false` a new memory.

### 0.0.7 — 2026-07-22

- Pin-only bump: rebuilt against `topodb` 0.0.10 / `topodb-json` 0.0.7. Minor
  doc clarification in the engine-error → exit-code mapping.

### 0.0.6 — 2026-07-20

- Dependency pins only: `topodb` 0.0.9, `topodb-json` 0.0.6. No functional change.

### 0.0.5 — 2026-07-18

#### Added

- **`find --normalized`**: case- and whitespace-insensitive matching for string values
  (`"drew powell"` finds `"Drew Powell"`) via the engine's new `nodes_by_prop_normalized`;
  the default stays byte-exact.

#### Changed

- **`link` normalizes edge types** through the shared `topodb_json::normalize_edge_type`
  (lowercase; whitespace/hyphens collapse to `_`), matching the MCP `link` tool and the batch DSL.
- **Stock-spec auto-upgrade on open** (same behavior as `topodb-mcp`): a db still on an older stock
  default spec is upgraded to the current default — adding the `(Entity, name)` text index — with a
  one-time reindex; customized specs are inherited verbatim.

### 0.0.4

> **Opening a database with this version migrates it, one-way.** This release embeds `topodb`
> 0.0.7, whose on-disk format is v4. The first `topodb` command against an existing v1/v2/v3
> database file auto-migrates it to v4, and older builds can no longer read it. Back up the
> `.redb` file first if you may need to roll back. See the `topodb-mcp` 0.0.6 note for the
> two-dimensions-per-model migration caveat — it applies here identically.

#### Changed

- Embeds `topodb` 0.0.7 (format v4) and `topodb-json` 0.0.4. No CLI surface changes.

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
