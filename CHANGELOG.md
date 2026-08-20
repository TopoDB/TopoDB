# Changelog

All notable changes to the packages in this repository are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Packages in this
workspace are versioned and released independently (tags are per-package, e.g.
`topodb-mcp-v0.0.4`), so each package has its own section below.

> **This changelog starts at the releases below.** Earlier versions predate it and are not
> reconstructed here — the git history is the record for those. A changelog that guesses at its
> own past is worse than one that says where it begins.

---

## Unreleased

### Added

- **Context warehouse** (`topodb-warehouse`, spec 2026-08-18): append-only, content-addressed log of raw session artifacts + mirrored engine ops under `<db>.warehouse/`; deterministic derive to `Artifact`/`Chunk` nodes with `evidence` lineage to memories; `rebuild` a redb from segments; hot/warm/cold/expired tiering; redaction. `topodb warehouse {status|drain|derive|tier|rebuild|verify|show}`; `[warehouse]` + `schedule.warehouse_*` in `.topodb.toml`; hygiene tasks drain (light), derive (heavy, daemon), tier (light); op-log compaction is clamped to the mirrored watermark; `db_info.warehouse`. Claude Code plugin: `warehouse-capture` PostToolUse hook + session/memory-write markers (`TOPODB_WAREHOUSE=0`, `TOPODB_WAREHOUSE_DIR`).
- `topodb-json`: stock text index now includes `(Chunk, text)`; the previous stock default upgrades automatically.
- `topodb graph` (CLI) and `graph_snapshot` (MCP): deterministic graph export —
  ego view (traverse from seeds and/or a search query, hop-labeled) or whole-scope
  view — as canonical JSON, DOT, Mermaid, or a self-contained interactive HTML
  file (zero network requests). Truncation is always reported, never silent.
  Snapshots are stamped with the op-log seq, not wall-clock time: same DB state,
  same bytes.

### Cursor plugin (`plugins/cursor`, 0.1.0)
- New Cursor plugin with the seamless memory tier: daemon-backed `topodb` MCP
  server (npm-bootstrapped, no Rust), chat-start recall injection, episode
  capture, context-warehouse capture, once-per-session stop nudge, rules nudge,
  `topodb-memory` skill, `/recall` and `/remember`. Installable from this repo's
  root `.cursor-plugin/marketplace.json`.
- Shares the database, scopes and daemon with the Claude Code plugin when both
  are installed (data dir: `TOPODB_PLUGIN_DATA` → `CLAUDE_PLUGIN_DATA` →
  `~/.claude/plugins/data/topodb-topodb/` if present → `~/.topodb/plugin-data/`).
- Known gaps: no subagent recall injection in Cursor; multi-root workspaces use
  the first root.

### Plugin core (`plugins/core`)
- Client-agnostic plugin code extracted into `plugins/core` and vendored into
  each plugin by `scripts/sync-plugin-core.mjs` (drift-checked in CI). Claude
  Code plugin 0.1.9: internal restructure only. Spool events now carry
  `harness: "claude-code" | "cursor"`; episodes carry `usage_judged`.

---

## `topodb` (engine)

### 0.0.18 — 2026-08-16

#### Added

- **Public `Db::get_meta`/`Db::set_meta`** over the engine's META table — a
  small typed key/value surface on the durable META store, so callers outside
  the engine (the onboarding hygiene catch-up) can persist and read
  bookkeeping (last-run timestamps, lifecycle-candidate counts) without a
  side file. Reserved engine keys (`format_version`, `hnsw_params`,
  `index_spec`) remain engine-owned.

### 0.0.17 — 2026-08-12

#### Changed

- Dependency-only version bump for the 0.0.19 release; no functional change.

### 0.0.16 — 2026-08-12

#### Added

- **`Op::UpsertNode`** — atomic find-or-create by an equality-indexed prop,
  resolved inside the applier's transaction (so it sees prior committed and
  prior same-group-commit state) and rewritten to a plain `CreateNode` (or
  dropped, remapping its id onto the surviving node and its same-batch edges)
  before the op log is appended. Concurrent writers naming the same entity now
  collapse onto the first-committed node instead of fragmenting the graph into
  duplicates. Appended LAST in the `Op` enum so it never shifts the postcard
  discriminants of the persisted variants (existing `.redb` logs decode
  unchanged; the variant never reaches the log). `AppliedBatch` gains `remap`
  (`planned → surviving` node ids) so callers can report the canonical id.

### 0.0.15 — 2026-08-11

#### Added

- **Bi-temporal edges** — `EdgeRecord` gains `recorded_at: i64` /
  `superseded_at: Option<i64>` (BELIEF time: when the edge was
  written/stopped being believed; stamped by the engine, never
  caller-settable), alongside `valid_from`/`valid_to` which are now
  documented as pure WORLD time (link/close_edge overrides unchanged).
- **`TimeAxis`** (`Valid` | `Recorded`, default `Valid`) on
  `TraversalQuery.time_axis` and as a new parameter of
  `Db::edges_from`/`edges_to` — `Recorded` answers "what did we believe at
  t"; a late-recorded fact (backdated `valid_from`) differs between axes.
  The `Valid` axis is behavior-identical to pre-v9 `as_of`.
- **Allen-style interval predicates (`ValidInterval`)** — new public enum
  (`During`/`Overlaps`/`Before`/`After`) over the half-open edge valid
  interval `[valid_from, valid_to)` (an open edge is unbounded on the
  right, so it never satisfies `During` or `Before`; an EMPTY interval —
  `valid_to <= valid_from`, creatable by a backdated close — was never
  valid at any instant and satisfies NO predicate, keeping `Overlaps
  [a,b)` exactly the union of `as_of` points in `[a,b)`), plus
  `ValidInterval::from_parts` — the single shared fold of the four
  optional surface params into at most one predicate (mutual exclusion,
  inversion, positivity), used by every host instead of per-surface
  ladders — with new reads
  `Db::edges_from_interval`/`Db::edges_to_interval` and
  `Db::traverse_interval` — the interval-vs-interval questions ("which
  edges were valid during Q2", "what overlapped the migration window")
  that the point-in-time `as_of` cannot express. The predicate REPLACES
  the `open_only`/`as_of` gate (valid axis only: `as_of` present or
  `time_axis: Recorded` is `Rejected`, as is an explicit `open_only`
  alongside a predicate at surfaces that express one), gates on the
  adjacency entries' interval fields (no extra record fetches, no access
  bumps, oldest-first order preserved), and is pure query-time
  mechanics — no format change; a v9 file opens with zero migration work.
- **`RecallQuery.corroboration_weight`** — RRF corroboration boost:
  post-fusion, each fused score is multiplied by `1 + w·(legs_hit − 1)/2`,
  where `legs_hit` counts the legs that actually RAN (text/vector/graph; a
  zero-weight leg is skipped entirely and never counts; expansions live
  inside the text leg). The graph leg counts a hit when the PPR list holds
  it OR when it is a seed 1-hop adjacent to ANOTHER seed — PPR excludes
  its seeds by contract, and without the co-seed rule the top hits
  (exactly what a tie-breaker is for) could never be graph-corroborated
  while their own neighbors could. Counting only; fusion inputs are
  untouched. A mild re-ranker that breaks near-ties toward hits
  corroborated across legs — RRF's additive term already favors agreement,
  and no recall-quality win beyond tie-breaking is claimed. Validated
  finite in `0.0..=1.0` (same envelope and error style as `access_weight`);
  the default `0.0` skips the code path entirely, so results are
  byte-identical to before (differential-tested, same pattern as the
  recency knob). **Breaking for struct-literal construction** of
  `RecallQuery` (new field) — same caveat as `label_weights`; use
  `RecallQuery::new(..)` with struct-update syntax.

#### Changed

- **Format v9 (ONE-WAY, in place): back up before first open.** Opening a
  v8-or-older file rewrites every EDGES row (postcard is positional) and
  every stored CreateEdge/CloseEdge op, backfilling the belief axis by the
  copy rule `recorded_at := valid_from`, `superseded_at := valid_to` —
  exact for every edge that was never explicitly backdated, an
  approximation for backdated ones. Runs in one transaction (crash-safe:
  nothing persists until commit).
- **`EdgeRecord` gains two public fields** (BREAKING for full struct
  literals without `..`-spread, same caveat as `SearchOptions` in 0.0.14).
- **`Op::CreateEdge` / `Op::CloseEdge` gain trailing optional fields**;
  stored logs are rewritten by the v9 migration so one canonical op shape
  decodes everything. `get_changes` output carries the new fields.

### 0.0.14 — 2026-08-10

#### Added

- **`SearchOptions.created_range`** — optional created-time filter
  (`CreatedRange { after_ms, before_ms }`, after inclusive / before
  exclusive), filters before top-k on both search paths; `None` unchanged.

#### Changed

- **`SearchOptions` gains a new public field, `recency_half_life_by_prop`**
  (BREAKING for callers building a full `SearchOptions` struct literal
  without `..Default::default()`) — optional prop-keyed recency half-life
  map (per-node decay curves). `None` (via `Default`) is unchanged flat
  behavior.
- **`SearchOptions` gains a second new public field, `created_range`** (same
  BREAKING-for-full-struct-literals caveat as above). `None` (via `Default`)
  is unchanged behavior.

### 0.0.13 — 2026-08-08

#### Added

- **Deterministic HNSW vector index (F8, on-disk format v7)** — `search_vector`
  (and hybrid recall's vector leg) now routes clusters that crossed a build
  threshold (default 1024 vectors per `(model, scope)`) through an HNSW graph
  instead of the brute-force scan; sub-threshold clusters and `candidates`
  queries keep the exact scan bit-for-bit. Approximation affects only which
  candidates surface — scores stay exact cosine and the public
  `(score desc, NodeId asc)` order is unchanged, with recall@10 ≥ 0.95 gated
  in tests. The graph is fully deterministic (integer-only level function,
  slot-ascending tie-breaks, op-order insertion) and rebuilt byte-identically
  by op-log replay, proptest-pinned. Deletes tombstone (waypoints stay
  routable); a 30% stale ratio triggers an inline cluster rebuild; graph
  parameters are stamped in META (`hnsw_params`) and reconciled on open —
  a changed override drains and rebuilds, a corrupt stamp errors. Format
  v6 → v7 migration is O(1) (two new tables, no data pass; graphs build
  lazily). Public API unchanged; `DbOptions.hnsw_params` is the only new
  surface. Release-mode benchmark gates are recorded as pending in
  BENCHMARKS.md (dev-machine disk blocked release builds at merge time).
  Neighbor selection is the diversity heuristic with keep-pruned-connections
  (params `version: 2`; the first 100k gate run caught closest-M collapsing
  recall to 0.078 on the uniform fixture — connected but not navigable).
  A `version: 1` closest-M stamp mismatches on open and those graphs drain
  and rebuild via the params reconcile; CI-scale recall (2000×32d) moved
  0.9660 → 0.9820. Graph ops memoize decoded vectors for their own duration
  (`VecCache` — one insert/rewire/query re-asks for the same slots across
  levels, selection, and pruning; ~40% of build time was redundant b-tree
  navigation + postcard decode), measured 1.83× insert throughput at
  20k×384 with byte-identical graphs.
- **Format v8: scalar-quantized vectors (SQ8)** — embeddings are stored as signed 8-bit max-abs codes plus scale (~4× smaller vector storage); scoring uses symmetric integer cosine on both scan and HNSW paths, enabling faster inserts and graph builds. Existing files migrate in place on open; HNSW graphs rebuild (params v3). Cosine scores are now computed over quantized codes; `NodeRecord.embedding` returns the dequantized (≈original) vector.

### 0.0.12 — 2026-07-27

#### Added

- **`SearchOptions.prop_retain`** — a mechanism-only string-prop allowlist (`PropRetain { prop, any_of, absent_as }`): a candidate survives iff its `prop` value (a missing/non-`Str` value reads as `absent_as` when set) is in `any_of`. Filtered before top-k in every text-search path, never access-bumped when dropped, and re-applied post-fusion in `recall` so vector/graph-leg candidates cannot leak past it. The engine names no prop and no vocabulary; empty `prop`/`any_of` are rejected.
- **`RecallQuery.tombstone_props` / `Db::search_text_live` takes a prop SET** — tombstone filtering generalizes from one prop (`tombstone_prop: Option<String>`) to any number (`tombstone_props: Vec<String>` / `&[&str]`): a candidate is dropped when ANY listed prop holds an `Int` timestamp `<=` the query's effective now. Per-prop semantics unchanged (future marks kept, non-`Int` values ignored, filtered before top-k, never access-bumped). **Breaking for struct-literal construction** of `RecallQuery` and for `search_text_live` callers — pass `vec![prop]` / `&[prop]` for the old behavior.
- **`Db::edges_to`** — incoming-edge read mirroring `edges_from`. Scoped listing of a node's incoming edges, filterable by source, edge type, and open-only.
- **`Db::search_vector_unbumped`** — cosine vector search with the same population, order, and scores as `search_vector`, but WITHOUT bumping access counters. For maintenance and advisory reads (near-duplicate checks) that must not spend the recency signal they exist to protect — the same rationale as `search_text_unbumped`. One shared implementation with the bumping variant.
- **`Db::search_text_live`** — `search_text_with` plus a liveness filter: a candidate whose named tombstone prop holds an `Int` timestamp `<=` "now" (`options.now_ms`, else wall clock) is dropped BEFORE the top-`k` truncation — a retired hit never consumes the result window — and is never access-bumped. Same tombstone semantics as `RecallQuery.tombstone_prop` (a mark in the query's future keeps the node; a non-`Int` value is not a mark), now available to plain BM25 callers.

### 0.0.11 — 2026-07-23

#### Added

- **`RecallQuery.label_weights`** — post-fusion per-label score multipliers
  (`Vec<(String, f32)>`), validated and applied multiplicatively alongside recency/access weighting
  in a single re-sort. Default empty (`vec![]`) changes nothing (byte-identical behavior by
  construction, no-op at defaults contract preserved). Enables host policies like down-weighting
  Entity hits relative to Memory for question-shaped queries. **Breaking for struct-literal
  construction** (new field) — use `RecallQuery::new(scopes, query, k)` with struct-update
  syntax so future additions don't break call sites.
- **`Db::search_text_unbumped`** — BM25 text search with the same population, order, and
  scores as `search_text`, but WITHOUT bumping access counters. For maintenance and advisory
  reads (hygiene scans, near-duplicate checks) that must not spend the recency signal they
  exist to protect — the same rationale as `nodes_by_label_unbumped`. One shared
  implementation with the bumping variant.
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

### 0.0.15 — 2026-08-16

#### Changed

- Dependency-only version bump for the 0.0.20 release; no functional change.

### 0.0.14 — 2026-08-12

#### Changed

- Dependency-only version bump for the 0.0.19 release; no functional change.

### 0.0.13 — 2026-08-12

#### Added

- `apply_upsert_remap` — rewrites a `remember`/`create_entity` result's entity
  ids and `created` flags against an applier upsert remap, so a caller never
  gets an orphaned planned id when a concurrent writer created the entity first.
- `lock_wait_budget_ms` + `DEFAULT_LOCK_WAIT_MS` — the shared
  `TOPODB_LOCK_WAIT_MS` parse-and-default policy, single-sourced across the CLI,
  the stdio MCP server, and the daemon so they cannot drift.

#### Changed

- `plan_remember` emits `Op::UpsertNode` (not `CreateNode`) for a newly created
  entity, so concurrent same-name writes resolve atomically at apply time.

### 0.0.12 — 2026-08-11

#### Added

- **`edge_believed_at`** — the belief-axis (recorded) liveness predicate,
  twin of `edge_live_at`; `edge_to_json` output gains `recorded_at`
  (number) and `superseded_at` (number|null).
- **`decision` memory kind** — the closed vocabulary `MEMORY_KINDS` grows
  to four (`episodic | semantic | procedural | decision`), giving "what
  did we decide about X, and what similar decisions preceded it?" a
  filterable vocabulary; `validate_memory_kind` accepts it and its error
  message lists all four. Half-life: new
  `LIFECYCLE_HALF_LIFE_DECISION_DAYS` `= LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS`
  (120d — a deliberate tie: decisions age like standing facts; change
  independently if dogfooding shows otherwise), wired EXPLICITLY rather
  than by fall-through: its own arm in the `lifecycle_candidates` kind
  match and its own entry in `memory_kind_half_life()`, with the
  anti-drift mirror test extended to enforce that all four kinds are
  covered. `MEMORY_KIND_DEFAULT` stays `semantic`; dedup stored-kind-wins
  and the `kind`-is-a-reserved-prop rejection are unchanged.
  `LifecycleParams` gains a `half_life_decision_ms` tunable (defaulted
  from the constant, positive-validated like the other three; BREAKING
  for full struct literals) so the sweep and the ranking map draw the
  decision half-life from the same constant — editing it can never move
  one and silently not the other. Stored as the
  existing `kind` Str prop — no disk-format change — and accepted
  immediately by every host (CLI `--kind`, MCP `remember` /
  `create_memory`, Obsidian frontmatter `kind`) through this shared
  validation.

### 0.0.11 — 2026-08-10

#### Added

- **`parse_temporal_query` / `parse_iso_instant` / `TemporalRewrite`** —
  deterministic regex-first temporal phrase parser (ISO date/month/year,
  before/after/since/until, between, yesterday/today/last week/month/N days;
  bare years require in/on/during; conservative — returns `None` without a
  parseable date). Note the new `regex` dependency.
- **`dup` module** — the duplicate/supersession classifier
  (`containment_of_sets`, `tokens`, `dup_band`, `text_dup_band`,
  `dup_relation`, `is_supersession`, plus the calibrated near-duplicate
  threshold constants) is now public here instead of private inside
  `topodb-mcp`, so `topodb-cli` can run the same text-mode classification
  without an embedder.
- `memory_kind_half_life()` — canonical kind→half-life map for ranking,
  built from the lifecycle constants (episodic 14d / semantic 120d /
  procedural 365d; semantic is the default bucket).

### 0.0.9 — 2026-07-27

#### Added

- **`lifecycle_candidates` + `staleness`** — the Phase C decay sweep: rank live memories by kind-aware staleness (`(age/half_life)/ln(e+access_count)`, age since last access falling back to ULID mint time), top-N (default 20) with full evidence `{id, content, kind, created_at, last_accessed_at, access_count, staleness}`. Deterministic under an injected `now_ms`, read-only, unbumped (built on `nodes_by_label_unbumped` + `access_stats`). Half-life defaults: episodic 14d, semantic 120d, procedural 365d (`LIFECYCLE_HALF_LIFE_*_DAYS`); absent/unknown kind uses the semantic half-life.
- **`plan_supersede` is now public** — layered callers (e.g., obsidian vault ingest) can compose supersession ops alongside remember, directly instead of via the batch DSL.
- **Memory `kind` taxonomy vocabulary** — `MEMORY_KIND_PROP` (`"kind"`), the closed enum `MEMORY_KINDS` (`episodic | semantic | procedural`), `MEMORY_KIND_DEFAULT` (`semantic` — what an absent prop reads as; no migration needed), and `validate_memory_kind`. `RememberRequest.kind` stamps NEW memories only (dedup ignores kind; the hit's stored kind wins); `kind` joins the reserved props rail with a message pointing at the parameter.
- **`MEMORY_FORGOTTEN_AT_PROP` + `MEMORY_TOMBSTONE_PROPS`** — the second tombstone and the canonical liveness set every surface filters on. `plan_forget(db, write_scope, ids, now_ms)` — strict shared ops builder for the `forget` verb (stamp + close open edges; ANY invalid id rejects the whole call, unlike `plan_supersede`'s skip-if-retired). `memory_props` now also rejects `forgotten_at` as reserved; `existing_memory` excludes forgotten nodes from dedup.
- **`plan_purge`** — the Phase E reclamation planner: selects Memory nodes whose any tombstone (`superseded_at`/`forgotten_at`) is an `Int` strictly older than the cutoff and returns `RemoveNode` ops + the ascending id list. Planning is separate from submitting so callers can dry-run; boundary values survive, non-`Int` marks and live nodes are never selected.

#### Changed

- **Batch unknown-op error hints** — when an unknown `op` field is encountered in a batch command, the error message hints that ops use underscore names (e.g. `create_memory` not `createMemory`).
- **`open_with_busy_retry` audible note** — after 500ms of retrying on `TopoError::Busy`, one stderr note is printed: `topodb: database held by another process; retrying (budget <N>ms)`.

### 0.0.8 — 2026-07-23

#### Changed

- **`existing_memory` no longer matches superseded memories** — re-storing identical content from a
  memory marked `superseded_at` creates a fresh new memory instead of deduping to the retired
  tombstone. The tombstone keeps its `as_of`-historical visibility and `content_hash` but is never
  returned as a dedup hit for the live write path. Reflects the fact pattern: if you're submitting
  a fact again, even if the wording matches a superseded memory, the fact is live again and deserves
  a live node — the old one is history.

#### Added

- **`edge_live_at(e: &EdgeRecord, t: i64)` shared predicate** — determines whether an edge
  is "live" (open) at a given query time (returns `true` if `valid_from <= t < valid_to`).
  Utility used by the CLI `get-edges` command and the MCP `get_edges` tool for temporal edge
  filtering. An edge with no `valid_to` (open edge) is live at any time >= `valid_from`.
- **`memory_props` constructor** — the shared new-Memory props builder used by both CLI and MCP
  servers. Rejects the system-maintained `content_hash` and `superseded_at` keys (caller-level
  validation, fails fast with an actionable error) so front ends can validate early without
  reaching the storage layer.
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

## `topodb-obsidian`

### 0.0.7 — 2026-08-16

#### Changed

- Dependency-only bump (engine 0.0.18 / json 0.0.15); no functional change.

### 0.0.6 — 2026-08-12

#### Changed

- Dependency-only bump (engine 0.0.17 / json 0.0.14); no functional change.

### 0.0.5 — 2026-08-12

#### Changed

- Dependency-only bump (engine 0.0.16 / json 0.0.13); no functional change.

### 0.0.4 — 2026-08-11

#### Changed

- **Frontmatter `kind` rejection message derives from `MEMORY_KINDS`** —
  it now names every kind in the shared vocabulary (including the new
  `decision`) instead of a hand-listed three; a future kind can never be
  missing from this message again.

### 0.0.1 — 2026-07-27

#### Added

- **`topodb-obsidian`** — new crate: Obsidian-format vault ingest/seed (note⇄memory mapping, wikilinks→entities, supersession on edit, fixpoint-tested round-trip).

---

## `topodb-okf`

### 0.0.2 — 2026-08-16

First published release of the crate.

#### Added

- **`topodb-okf`** — new crate: Open Knowledge Format (OKF v0.2) bundle ⇄ graph.
  Ingests an OKF bundle into the property graph (notes → memories, mappings →
  entities/links) and seeds a bundle back out, with a report of what was
  created vs. matched. Built on the shared `topodb`/`topodb-json` surfaces.

---

## `topodb-mcp`

### Unreleased

#### Added

- **Scheduled reingest of configured sources** — `[[reingest.source]]` entries
  in `.topodb.toml` (`kind = "obsidian" | "okf"`, `path`, optional `scope`) are
  re-ingested by the resident daemon's `allow_heavy` hygiene tick. Source paths
  resolve against the config file's directory (`~` expands to the platform
  home). Text-only in v1 (no embedder). Each source is attempted independently:
  a whole-source failure (missing path, walk error) is surfaced in the report,
  never fatal, and `reingest`'s `last_run` advances after attempting all, so a
  misconfigured path retries next interval rather than every tick. `topodb init`
  resolves the same sources but leaves the heavy work deferred.

### 0.0.20 — 2026-08-16

#### Added

- **`onboarding_pointer` tool** — returns the canonical CONVENTIONS pointer
  text and its version, so a client (Claude Code, Pi) can fetch the pointer to
  inject into `CLAUDE.md`/`AGENTS.md` without hard-coding the wording. This is
  the 32nd tool; clients that pin an exact tool count must move to 32.
- **Server-startup onboarding** — on boot the server ensures a `CONVENTIONS.md`
  exists and runs a bounded hygiene catch-up (compaction / purge / lifecycle
  candidates, each gated on its own due computation), then persists last-run
  state via the engine's META store. Runs in stdio mode and, as of this
  release, also in socket/daemon mode (the Claude Code path), so the daemon
  isn't skipped.
- **Daemon hygiene tick** — a best-effort periodic hygiene pass while the
  daemon is resident, so a long-lived session keeps its store tidy without a
  separate cron.

### 0.0.19 — 2026-08-12

#### Added

- **Windows named-pipe transport** — the resident daemon (`--socket`) now serves
  on Windows over a named pipe, not just unix domain sockets, so Windows gets the
  same concurrent multi-agent daemon (per-connection rmcp sessions on one shared
  `Db`, redb-lock election, idle-exit, hello handshake). The connection handler
  is transport-generic; Windows needs no stale-inode reclaim (named pipes vanish
  on process exit). Verified by a named-pipe smoke test on CI.

### 0.0.18 — 2026-08-12

#### Added

- **Resident daemon** (`topodb-mcp --socket [PATH]`) — serves the database over
  a per-user unix socket so many agents (parallel subagents in one session AND
  independent processes: other sessions, Bash-spawned CLI calls, hooks) read and
  write one `.redb` at once. Each connection is its own rmcp session on one
  shared `Db`: reads run concurrently on redb's MVCC, writes funnel into the
  existing group-commit applier. Lifecycle: redb-lock election (only the winner
  binds the socket), idle-exit after `TOPODB_DAEMON_IDLE_MS` (default 60s, `0` =
  never), stale-socket reclaim, and a `topodb/hello` scope handshake with a
  per-connection timeout and a `-32002` refusal (wedge defense). The socket
  endpoint (`topodb-v1-<sha12>.sock`) is derived identically to the plugin's
  `ipc.js`. Runs alongside the unchanged stdio mode; Windows named-pipe
  transport is a follow-up (unix-only for now).
- `remember` and `create_entity` emit `Op::UpsertNode`, so concurrent writers
  naming the same entity no longer fragment the graph into duplicate nodes.

### 0.0.17 — 2026-08-11

#### Added

- **`time_axis` on `get_edges` / `traverse`** — `"valid"` (default,
  unchanged behavior) or `"recorded"` ("what did we believe at as_of");
  edge results carry `recorded_at` / `superseded_at`. Payload ceiling
  re-based to 80,000 bytes for the new axis docs (measured 79,488).
- **Interval predicates on `get_edges` / `traverse`** — four optional
  params over the edge valid interval: `valid_during: [a, b]`,
  `valid_overlaps: [a, b]`, `valid_before: t`, `valid_after: t` (Unix ms,
  half-open `[a, b)`). At most one may be set; each is mutually exclusive
  with `as_of`, explicit `open_only`, and `time_axis: "recorded"` — a
  predicate REPLACES the temporal gate. Folded and validated by the
  engine's shared `ValidInterval::from_parts`.
- **`search_memories.corroboration_weight`** — host default `0.2` (mild:
  max ×1.2), `0` disables, engine default stays off; boosts hits present
  in multiple recall legs (see the engine entry for the counting rule,
  including co-seed graph evidence).
- **`decision` memory kind across the surface** — `remember` /
  `create_memory` accept `kind: "decision"`, the `search_memories.kinds`
  filter takes `"decision"`, staleness uses its 120d bucket; server
  instructions document the convention: rationale in the content, link to
  affected entities, precedent via `kinds: ["decision"]`, causal edge
  types `caused_by` / `influenced`.

### 0.0.16 — 2026-08-10

#### Added

- **`search_memories` temporal filters** — `created_after` / `created_before`
  (ISO date strings, period-start); `temporal_rewrite` (default `true` — date
  phrases become created-time filters, residual searched; explicit params
  always win); `applied_time_filter` result echo (the derived interval).
- **`link.conflicts`** — after a write that did not pass `supersede: true`,
  `link`'s result lists OTHER open same-type edges from the same node
  (`{edge_id, to, valid_from}`), omitted when empty. Advisory only — no
  edge is closed or altered by this scan.
- **`remember.check_conflicts` + `remember.supersession_candidates`** — a
  new `check_conflicts` param (default `true`) gates `remember`'s existing
  write-time near-duplicate probe; the result gains a leaner
  `supersession_candidates` field (`{memory_id, relation, score}`) derived
  from the same probe as `near_duplicates`, omitted when empty or when
  `check_conflicts` is `false`. No new search is added.

#### Changed

- **`search_memories` queries containing date phrases are now time-filtered by
  default** (`temporal_rewrite` defaults to `true`) — pass
  `temporal_rewrite: false` for verbatim search. Machine-constructed queries
  built from prompt text should always pass `false` (the plugin's
  subagent-priming hook does).
- **`search_memories` tool description trimmed** — the 7× duplicated `scopes`
  param doc consolidated.
- **`check_conflicts: false` also suppresses the existing `near_duplicates` field**
  (both derive from the same probe).
- **`search_memories` recency is kind-aware by default** — episodic memories
  decay faster, procedural slower, semantic (and unstamped) in between;
  previously every memory used the same flat half-life. Pass an explicit
  `recency_half_life_days` to restore a flat prior over all kinds (D2 fix).

### 0.0.14 — 2026-07-27

#### Added

- **`lifecycle_candidates` tool** (29 tools) — the decay sweep over MCP: same shared builder and semantics as the CLI subcommand (deterministic `now_ms`, tunable half-life days, unbumped, read-only). The description carries the lifecycle doctrine: the sweep proposes; the agent reviews each candidate's evidence and acts via `forget`/`consolidate_memories`.
- **`remember.kind` + `search_memories.kinds`** — the kind taxonomy over MCP: `kind` enum-validates and stamps new memories (dedup ignores it; the stored kind wins), `kinds` filters recall post-fusion with absent-as-`semantic` (entity hits count as `semantic` — combine with `labels: ["Memory"]` to filter to memories only). Invalid or empty values are `invalid_params`.
- **`forget` tool** — soft-retire memories (`ids`, optional `scope`): stamps `forgotten_at` + closes open edges atomically via the same shared planner as the CLI. Strict targets: any invalid id rejects the whole call. Distinct from `remember.supersedes` (replacement) — forget is "never needs to come back".
- **Liveness is now the shared tombstone set** — `search`, the dedup path, the near-duplicate advisory, and the hygiene scans all treat `forgotten_at` exactly like `superseded_at`.
- **`get_edges` — `direction` parameter** (enum: `"out"`/`"in"`/`"both"`, default `"out"`). For `"out"` (default), lists the node's outgoing edges as before. For `"in"`, the anchor shifts to the target and `to_id` filters the far source end (incoming edges, mirrored view). For `"both"`, returns an id-deduped union of incoming and outgoing edges.
- **`ingest_vault` / `seed_vault`** — vault bridge tools; tool count 28 → 30.

#### Fixed

- **Text-mode near-duplicate detection improvements** — the lexical duplicate-vs-supersession classifier now handles short token sets correctly: band is capped at "possible" (not "likely") when the smaller memory's token set has fewer than 6 tokens (`TEXT_BAND_MIN_TOKENS = 6`); negation-cue windows now count content tokens (stopwords and cues don't consume slots) and are clause-bounded at sentence marks (`.,;:!?()`); sentence-initial "never …" contradictions (e.g. "use the staging db" vs "never point load tests at the staging db") now correctly classify as supersession instead of duplicate; "at" added to the classifier stopword list.
- **Write-time near-duplicate advisory no longer bumps access counters** — the MCP advisory check on write now uses `Db::search_vector_unbumped` instead of `search_vector`, so advisory reads no longer corrupt the staleness signal that memory-hygiene sweeps rely on to detect stale content.
- **`containment_of_sets` returns 0.0 for empty sets** — when exactly one token set is empty, `containment_of_sets` now returns 0.0 (deliberate, well-defined value) instead of NaN.

#### Changed

- **Text near-duplicate scoring** — switched from Jaccard (`|A∩B|/|A∪B|`) to token containment (`|A∩B|/min(|A|,|B|)`), floor 0.7 (was 0.6). The field test's canonical contradiction pair (similarity ≈0.833) is now correctly caught as band `"likely"`. In text mode (`find_duplicate_memories`, `memory_health` when embedder is not Ready), text-mode `similarity` field is now a containment score (not cosine, not Jaccard).
- **`consolidate_memories` now requires both `keep` and `drop` to be live under the full tombstone set** — a `forgotten` id is now rejected the same way an already-superseded one is (previously only `superseded_at` was checked). The rejection message changed from `"... is already superseded"` to `"... is already superseded or forgotten"` — a note for anyone string-matching on it.

#### Release checklist

- **Bump the Claude Code plugin's server pin** (`plugins/claude-code/server-args.js`'s
  `SERVER_VERSION`, currently still `"0.0.13"`) to this version once it is published to npm —
  pin + the e2e devDependency move in the same commit, per the release rule; see
  `plugins/claude-code/README.md`'s "Server version" section for why the pin can't move early.
- **First crates.io publishes this release:** `topodb-obsidian 0.0.1` (must land BEFORE
  `topodb-cli`/`topodb-mcp`, which depend on it) and `topodb-sgh 0.0.1`. Full publish order:
  topodb → topodb-json → topodb-obsidian → topodb-cli → topodb-mcp → topodb-sgh.

### 0.0.13 — 2026-07-23

#### Added

- **`traverse` and `get_edges` — `as_of` params for temporal reads.** `traverse` accepts `as_of`
  (Unix milliseconds) to walk the graph at a historical timestamp (closed edges reappear, later
  edges vanish; a future `as_of` behaves like "now"); omit for "now". `get_edges` accepts `as_of`
  to list edges within the window `valid_from <= t < valid_to` (valid_to exclusive, inclusive
  lower bound); omit `open_only` when passing `as_of` (mutually exclusive — `as_of` already means
  "open at that instant").
  **Temporal dimension lives on edges; nodes are always current-state** — `get_node` readings
  remain timeless, focused on node labels and properties as they exist now.

#### Changed

- **`search_memories` defaults to down-weighting Entity hits (`label_weights: {"Entity": 0.5}`)** —
  facts (Memory nodes) now outrank bare entity handles for question-shaped queries. Pass an explicit
  `label_weights: {}` to disable and restore the old ranking (facts and entities equally weighted).
  Full per-label control available via the new `label_weights` param — factors are validated 0.0–10.0,
  default empty = unchanged behavior. Enables MCP hosts to implement semantic policies without
  modifying the engine.
- **`remember` and `create_memory` tools reject reserved props keys `content_hash`/`superseded_at`**
  in the `props` param (returns `invalid_params` error) — these are system-maintained and
  caller-settable only via dedup and supersession primitives, not arbitrary props.
- **Re-storing identical content from a superseded memory creates a new live memory** — `create_memory`
  and `remember` no longer dedup against superseded memories (tombstones). If you submit identical
  content for a retired fact, it mints a fresh memory; the old tombstone retains its history for
  `as_of`-past queries but never surfaces as a dedup hit.
- **Startup warning on unparseable `TOPODB_LOCK_WAIT_MS`** — if the env var is set but not a valid
  millisecond integer, the server now logs one stderr warning and falls back to the default (3000 ms)
  instead of silently ignoring the invalid value.
- **Batch DSL `#N` references are 0-indexed** — documentation clarified at every site (`submit_batch`
  tool description, README, `topodb-cli submit` doc comment, `topodb-json` batch module) to explicitly
  state that `#0` refers to the first command's produced id, `#1` to the second, etc. (doc-only).
- **Internal refactor**: `remember` and entity-lookup composition now delegates to the shared
  `topodb-json::compose` module instead of maintaining separate logic in the MCP server. The
  startup open path now retries on `TopoError::Busy` using the same `TOPODB_LOCK_WAIT_MS` env
  var. No tool-surface change.
- **Near-duplicate detection** applies whenever the embedder is not Ready (Failed, Downloading, or deliberately Off).
  `find_duplicate_memories` and `memory_health` scans apply lexical dup_relation (negation-cue heuristic) in all modes.
  **Vector mode** (embedder Ready): cosine similarity with contradiction detection; banded pairs (`likely`/`possible`).
  **Text mode** (embedder not Ready): token-Jaccard similarity at fixed 0.6 floor, exhaustive scan;
  `min_similarity` ignored (text scores incomparable to cosine); scans empty only when deliberately off (`--embeddings off`).
  Each pair carries a `method` field (`"vector"` / `"text"`), `relation` (`duplicate` / `supersession`),
  and `band` (omitted only in text mode). `memory_health` gains `degraded`/`degraded_reason` fields and forces
  `needs_attention: true` when the embedder is Failed or Downloading (text mode is degraded hygiene).
  Deliberately off (`--embeddings off`) is not degraded — the text fallback still runs; only the
  `degraded` flag distinguishes wanted-but-broken from chosen-off.

#### Fixed

- **`--help`/`-h` and `--version`/`-V` flags now print to stdout and exit 0** — previously these flags
  were not available. They now match conventional behavior for CLI tools.

#### Release checklist

- Publish to npm (automatic on the `topodb-mcp-v0.0.13` tag), then **bump the Claude Code
  plugin's server pin** (`plugins/claude-code/server-args.js`'s `SERVER_VERSION`) to 0.0.13,
  re-verify `plugins/claude-code/test/broker.test.js` against the real published package, and
  update the plugin's `SKILL.md`/commands for the 0.0.13 surface (as_of on traverse/get_edges,
  `label_weights`, hygiene `method`/`degraded`) **in the same commit as the pin bump** — the
  plugin must never document tools its pinned server doesn't have.

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

## `topodb-sgh`

### 0.0.8 — 2026-08-16

#### Added

- **`--agent-web`** — grants agent nodes a read-only web tool surface
  (fetch/search) for research fan-outs, alongside the existing `--agent-bash`.
- **`--agent-all-tools`** — grants agent nodes the full tool surface in one
  flag, for research/test fan-outs that need everything.

#### Fixed

- **`resume` advances all DAG waves** and refuses a silent non-interactive
  abort — a resumed run now drives every remaining wave to completion instead
  of stalling after the first, and surfaces a hard error rather than exiting
  quietly when it cannot proceed unattended.

### 0.0.7 — 2026-08-12

#### Changed

- Dependency-only bump (engine 0.0.17); no functional change.

### 0.0.6 — 2026-08-12

#### Changed

- Dependency-only bump (engine 0.0.16); no functional change. (Intermediate
  0.0.3–0.0.5 were undocumented dep-only bumps.)

### 0.0.2 — 2026-08-04

The first release of sgh as a provider-agnostic agent framework: everything
merged since the pre-framework 0.0.1 binary, in three arcs — the provider
framework (Phases 1–3), the follow-ups sweep, and distribution.

#### Added

- **Provider-agnostic execution** — agent nodes run over `--provider
  claude-code` (default, the local `claude` CLI), `anthropic` (API), or
  `openai` (any OpenAI-compatible endpoint via `--base-url`: vLLM, Ollama,
  etc.), selected per run behind cargo features (`claude-code`, `anthropic`,
  `openai`, `cli`; all default-on). One `HttpChatRunner` hosts thin provider
  codecs; retry/backoff policy is shared by construction. Replanning works on
  every provider.
- **Durable, resumable runs** — a shared-scope run index (`sgh show --list`),
  per-run status, the stored graph, and a JSONL event sidecar written as the
  run progresses. `sgh resume <run-id>` continues a crashed or halted run
  with the model-call bound held across resumes; `--approve-gate` records
  operator approvals durably; `sgh show <run-id> [--follow]` tails the event
  log without touching the (possibly locked) database.
- **Parallel executor** — `--max-inflight` runs independent ready nodes
  concurrently (CLI default 4; library default 1 is bit-identical to
  sequential), panic-safe, with `--agent-timeout` deadlines everywhere and
  cooperative Ctrl-C cancellation (cancelled runs exit 1).
- **Node-scoped MCP bridge** — the `topodb-mcp` child starts when a
  `tools: [topodb]` node begins and stops when the last one finishes, so the
  memory db's exclusive lock is released between tool-using nodes — `command`
  nodes can read the same db mid-run. A dead bridge child no longer fails
  every later node (the next tool-using node respawns it). A well-formed
  `--agent-mcp` command whose binary fails to start surfaces as that node's
  failure (run exit 1) instead of exit 2 at approval time.
- **npm distribution** — `npm i -g @topodb/topodb-sgh` installs the prebuilt
  `sgh` binary (published from `topodb-sgh-v*` release tags: a dependency-free
  launcher plus five platform sub-packages, exact-version pinned, mirroring
  `@topodb/topodb-mcp`'s channel).
- **`--model` is a flag rail for HTTP providers** — `--provider anthropic|openai`
  without `--model` exits 2 at flag time on `run`/`resume`/`plan`, next to
  the other rails, instead of failing at the first request after the approval
  gate and burning retry budget. `claude-code` keeps `--model` optional (the
  claude CLI's own default applies).
- HTTP transport on ureq 3 (the workspace carries a single ureq major); the
  transport contract (HTTP error statuses arrive as `Ok((status, body))` with
  the body readable; only transport failures are `Err`) is pinned by loopback
  tests.

#### Fixed

- **`sgh show --follow` no longer emits a garbled line after a hard IO read
  error** — the reader rewinds to the failed line's start before the error
  propagates, so the next poll re-reads a complete line.
- **The run index's `last_ms` high-water mark can no longer move backwards**
  under a clock that steps back mid-run (NTP correction, VM restore) — resume's
  clock floor is now genuinely non-decreasing.

### 0.0.1 — 2026-07-27

#### Added

- **`--agent-bash <prefix>`** — direct CLI flag to grant agent nodes Bash permissions by prefix (e.g. `--agent-bash 'topodb'` grants `Bash(topodb:*)` additively on top of Read/Write/Edit). The flag is only available for direct CLI use; the Claude Code plugin never passes it and runs agents under configured global permissions instead. Grant the narrowest binary scoped to your task (never shells or package managers). The approval gate echoes every grant before execution.
- **`--agent-mcp` + `tools: [topodb]`** — agent nodes can opt into the TopoDB
  MCP tool surface (`mcp__topodb`, full server). The run/validate flag supplies
  the server command (rail-validated, whitespace-split, no shell); the graph
  opts nodes in per-node; the approval gate echoes the server and the opted-in
  node ids. Nodes without the opt-in produce byte-identical `claude -p`
  invocations to before. A graph opting in without the flag fails validation
  at the gate, not mid-run.
- **Plugin `$SGH_MCP` helper** — `sgh-env.sh` now composes and exports `SGH_MCP`, the ready-made value for `sgh run --agent-mcp`: an absolute `topodb-mcp` binary (resolution mirrors `$SGH_BIN`: override, in-repo release, PATH, cargo bin dir) plus a per-project agent-memory database derived from `$SGH_DB` (`<same path>-memory.redb`, so an `SGH_DB` override keeps the pair together), with `--scope shared --embeddings off`. A missing `topodb-mcp` leaves `SGH_MCP` unset with a stderr note instead of failing — MCP is per-node opt-in, and graphs without opted-in nodes must keep working.

#### Fixed

- **`sgh validate` prints failures without a preceding success line** — the `--agent-mcp` rail and pairing checks now run before "valid: N node(s)" prints, so a failing validate no longer emits success-then-error.

---

## `@topodb/pi` (Pi extension)

### 0.0.7 — 2026-08-16

#### Changed

- **Bundles `@topodb/topodb-mcp` 0.0.20** (was 0.0.17) — the republished
  server with the `onboarding_pointer` tool (32 tools). The pinned tool count
  (`test/tool-count.ts`) moves 31 → 32 with the dependency, verified against
  the published server via `npm ci`.

### 0.0.6 — 2026-08-11

#### Added

- **Idle release: the resident `topodb-mcp` child is reaped after
  `TOPODB_IDLE_MS` (default 30s) of quiet**, freeing redb's exclusive lock
  so other processes (the CLI, another agent) can use the same db while a
  Pi session sits idle. The next tool call respawns lazily; an in-flight
  call is never reaped (the timer arms only when the last in-flight op
  completes); `TOPODB_IDLE_MS=0` keeps the old always-resident behavior.
  `list` now answers from the cached tool list without respawning an idled
  child. Mirrors the sgh `OnDemandBridge` lease design (#65).
- `TopodbServer.running` — whether a child is currently resident.
- **Failure-path hardening** (review findings on the idle-release commit):
  reaps are graceful (stdin-EOF first — the only shutdown that provably
  releases the redb lock through the wrapper/grandchild chain, notably on
  Windows — with a kill fallback after `killGraceMs`); a respawn awaits the
  previous child's full exit instead of racing it for the lock; `ensure()`
  is single-flight (concurrent cold callers share one spawn+handshake); a
  rejected handshake reaps the orphan instead of leaving it resident; a
  transport-level call failure (timeout, child death) reaps the wedged
  child while app-level errors keep it resident (sgh Tool-error
  semantics); the spawned child now receives the caller's env (env-only
  settings like `TOPODB_LOCK_WAIT_MS` silently didn't propagate before);
  `TOPODB_IDLE_MS` beyond Node's setTimeout range is clamped instead of
  inverting into reap-after-1ms; the cached tool list is invalidated on
  respawn (it could span server versions); and the episode flush retries
  once (`TOPODB_FLUSH_RETRY_MS`, default 2s) instead of silently dropping
  the episode to transient lock contention. The pi suite now runs in CI
  (node-test, both platforms).

#### Fixed

- README no longer hardcodes the proxied tool count (said "16", server has
  31 — the same count-drift class the 0.0.5 test centralization addressed).

### 0.0.5 — 2026-08-11

#### Changed

- **Bundles `@topodb/topodb-mcp` 0.0.17** (was 0.0.6 — eleven server
  releases of drift closed in one jump; ⚠️ an existing Pi-side `.redb`
  chain-migrates one-way to format v9 on first open). The bundled episode
  IndexSpec now unions the modern default spec (Alias/Synonym equality,
  `(Memory, content_hash)` dedup, Entity/Alias name text) with the
  episode-specific indexes — `remember`-path dedup and alias resolution
  work against Pi-created dbs.
- **Episode edge vocabulary is normalized-lowercase** (`issued` /
  `returned` / `used` / `used_policy`) — the server's
  `normalize_edge_type` lowercases on write, so the recorder now emits
  the stored form instead of relying on case that no longer survives.
- **Tests: tool-count pin centralized** (`test/tool-count.ts`, currently
  31) and every spawned server is reaped in `finally` — an assertion
  failure can no longer leak a child and hang the runner.

---

## Claude Code plugin

### 0.1.8 — 2026-08-16

#### Added

- **Install-time onboarding** — on session start the plugin injects the
  canonical CONVENTIONS pointer into the project's `CLAUDE.md` (fetched from
  the server's `onboarding_pointer` tool), even on an empty store, so a new
  agent learns the memory conventions without manual setup. The pointer's
  version is pinned to the server's `ONBOARDING_VERSION`.

#### Changed

- **Pins `@topodb/topodb-mcp` 0.0.20** (engine 0.0.18) — the republished
  server carrying the `onboarding_pointer` tool (32 tools) and server-boot
  onboarding that now runs in socket/daemon mode too (the plugin's path).

### 0.1.7 — 2026-08-13

#### Changed

- **Pins `@topodb/topodb-mcp` 0.0.19** and routes `launch.js` through the
  resident daemon on **all** platforms — Windows gets the named-pipe daemon,
  so `broker.js` leaves the launch path entirely (it remains only as the
  session-start/subagent-start hook-test stdio seeder).

### 0.1.6 — 2026-08-13

#### Changed

- **Pins `@topodb/topodb-mcp` 0.0.18** (resident daemon + atomic
  find-or-create `Op::UpsertNode`). Interim Windows broker-fallback so the
  Windows plugin keeps memory while the named-pipe daemon lands in 0.1.7.

### 0.1.5 — 2026-08-11

#### Changed

- **Pins `@topodb/topodb-mcp` 0.0.17** (engine 0.0.15). Real sessions now
  get: bi-temporal edges (format v9 — ⚠️ first open migrates a v8 db
  one-way; back up first), `time_axis` and the four `valid_*` interval
  predicates on `get_edges`/`traverse`, the `decision` memory kind with
  its precedent convention, and corroboration-boosted `search_memories`
  (default 0.2, `corroboration_weight: 0` disables).

### 0.1.2 — 2026-07-27

#### Added

- **`/sgh:show`** — list this project's sgh runs or inspect one run's event
  log, safely mid-run (`show <run-id>` reads only the event sidecar;
  `show --list` falls back to the sidecar directory when the db is locked).
  sgh plugin 0.1.1.
- **`/sgh:lifecycle` + the shipped lifecycle graph** (`graphs/lifecycle.yaml`) — the F6 Phase D reference loop: a deterministic `lifecycle-candidates` sweep (command node, no model call), a judge agent that reviews decay candidates plus `find_duplicate_memories` pairs and applies its verdicts via `mcp__topodb` (`forget`, consolidations first), and a verify command node that re-reads the db and fails the run if any claimed action is not reflected — self-reports cannot fake it. Two-step gate mirrors `/sgh:run`; requires `topodb`, `topodb-mcp` and `jq`.
- **`sgh-env.sh`: `SGH_MEMORY_DB` + `SGH_TOPODB`** — the per-project memory-db path is now exported once (and reused inside `SGH_MCP`), and the topodb CLI resolves with the established override → in-repo release → PATH → cargo-bin order (non-fatal on miss, like `SGH_MCP`).
- **`/topodb:vault-seed` / `/topodb:vault-ingest`** — working-memory vault commands.
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

### 0.1.1 — 2026-07-23

#### Changed

- **Server pin bumped to topodb-mcp 0.0.13** (`SERVER_VERSION` + the e2e devDependency, in the
  same commit per the release rule). Brings the plugin's server current with today's release:
  `as_of` temporal reads on traverse/get_edges, Entity down-weighting in `search_memories`
  (`label_weights`), superseded-content re-remember semantics, reserved memory prop keys, and
  text-fallback hygiene with `method`/`degraded` reporting. `SKILL.md` now teaches
  `remember`'s `supersedes` and `as_of` history reads.

---

## `topodb-cli`

### 0.0.15 — 2026-08-16

#### Added

- **`topodb init`** — scaffolds a store: writes `CONVENTIONS.md`, a
  non-clobbering `.topodb.toml` (config-only injection with a `[schedule]`
  block), and runs the onboarding hygiene catch-up. `--if-needed` always
  exits 0 (safe to call unconditionally from a hook); a failed daemon start
  is non-fatal.
- **`topodb conventions [--pointer]`** — prints the canonical CONVENTIONS
  document, or just the pointer text with `--pointer`, so a client can inject
  the same pointer the MCP `onboarding_pointer` tool returns.
- **Windows named-pipe client** — socket-first execution now works on Windows,
  not just unix. A `topodb <cmd>` routes to a resident `topodb-mcp --socket`
  daemon over its named pipe (opened as a blocking file, no new dependency and
  no async), closing the last Windows asymmetry: before this, a Windows CLI call
  made while a session's daemon held the database fell through to a direct open
  and failed `Busy`. `daemon status|start|stop` work on Windows too (`start`
  spawns the daemon detached via `creation_flags`). Verified by a `cfg(windows)`
  routing test on CI; unix behavior is unchanged.

### 0.0.14 — 2026-08-12

#### Changed

- Dependency-only version bump for the 0.0.19 release; no functional change.

### 0.0.13 — 2026-08-12

#### Added

- **Socket-first execution** — when a resident `topodb-mcp --socket` daemon
  holds the database, the CLI routes memory commands to it as MCP tool calls
  with output byte-identical to the direct in-process path (and falls back to a
  direct open when no daemon is present), so a `topodb` call co-exists with a
  session's daemon instead of failing `Busy` after the lock-wait budget. Adds a
  `topodb daemon status|start|stop` control surface. `search` routes to
  `search_memories` (richer recall ranking); `info`/`changes` stay direct-open.
- `remember`/`create-entity` emit `Op::UpsertNode` for concurrency-safe
  find-or-create (a concurrent same-name create no longer fragments the graph).

### 0.0.12 — 2026-08-11

#### Added

- **`--time-axis <valid|recorded>`** on `get-edges` and `traverse`
  (default valid, unchanged); `link` documents `--valid-from` as the
  world-time override; edge output carries the belief-axis fields.
- **`--valid-during a..b` / `--valid-overlaps a..b` / `--valid-before t`
  / `--valid-after t`** on `get-edges` and `traverse` — the same interval
  predicates as MCP (at most one; exclusive with `--as-of`, explicit
  `--open-only`, and `--time-axis recorded`), folded by the engine's
  shared `ValidInterval::from_parts`. Negative timestamps reach the
  engine's structured rejection in both `--flag value` and `--flag=value`
  forms.
- **`--kind decision`** accepted wherever kinds are (`remember`, kind
  filters) via the shared `topodb-json` vocabulary. NOTE: CLI `search`
  deliberately has NO `--corroboration-weight` — it is a single-leg text
  surface with no fusion legs; the corroboration boost is an MCP
  `search_memories` tunable.

### 0.0.11 — 2026-08-10

#### Added

- **`search` temporal filters** — `--created-after` / `--created-before`
  (ISO date strings, period-start); `--no-temporal-rewrite` (default rewrite
  on — date phrases become filters, residual searched); stderr time-filter
  echo when applied.
- **`search` — `--recency-weight` / `--recency-half-life-days` flags** —
  tune or disable (`--recency-weight 0`) the recency prior; passing
  `--recency-half-life-days` switches from the kind-aware default to a flat
  half-life over all kinds.
- **`link` / `remember` conflict parity** — `link`'s JSON output gains a
  `conflicts` field (other open same-type edges from the same node,
  omitted when empty); `remember`'s JSON output gains a
  `supersession_candidates` field (text-mode duplicate/supersession
  classification against existing memories — the CLI has no embedder, so
  this always runs in text mode), omitted when empty or on a dedup hit.
  Note: CLI `link` does not reuse identical edges, so `conflicts` may
  include the just-linked target (MCP reuses and never reports it).

#### Changed

- **`search` now applies the kind-aware recency prior by default**
  (BREAKING for score values in stdout, order-stable for same-age corpora) —
  episodic memories decay faster, procedural slower, semantic (and
  unstamped) in between, instead of one flat half-life for every kind.

### 0.0.9 — 2026-07-27

#### Added

- **`lifecycle-candidates`** — the decay sweep as a subcommand: `--limit` (default 20), per-kind `--half-life-*-days` flags, `--now-ms` for reproducible runs. Prints the ranked evidence array; read-only and unbumped. The sweep proposes — act on it with `forget`.
- **`remember --kind <episodic|semantic|procedural>`** — classifies a NEW memory; enum-validated (exit 2 otherwise); ignored on a dedup hit (the stored kind wins). Omitted = reads as `semantic`.
- **`search --kinds <kind>[,<kind>...]`** — only return hits of these kinds, on BOTH the default and `--include-superseded` paths; a node without a `kind` prop (including entities) counts as `semantic`. Filtered before top-k, unbumped.
- **`forget <id>...`** — soft-retire memories: stamps `forgotten_at` and closes their open edges atomically. Recall and default `search` stop returning them; history stays reachable (`search --include-superseded`, temporal reads). Every id must be a live Memory in the write scope — unknown, non-Memory, already-forgotten, or already-superseded ids reject the whole call (exit 2). Output: `{"forgotten": [ids]}`.
- **`get-edges` — `--direction out|in|both` flag** (default `out`). For `out` (default), lists the node's outgoing edges as before. For `in`, the anchor shifts to the target and `--to` filters the far source end (incoming edges, mirrored view). For `both`, returns an id-deduped union of incoming and outgoing edges.
- **`obsidian-ingest` / `obsidian-seed`** — vault bridge subcommands.
- **`purge`** — destructive space reclamation, completing F6: `--tombstoned-before <unix-ms>` hard-deletes long-tombstoned memories (engine remove-node, edges cascade). Dry-run by default (count + ids, nothing written); only `--yes` submits, atomically. Purged history is gone — `as_of` queries stop seeing those nodes; that is the point. CLI-only, and never part of the `/sgh:lifecycle` graph.

#### Changed

- **`search` now skips superseded memories by default** — a memory retired by `remember --supersedes` (an `Int` `superseded_at` prop in the past) no longer surfaces, consumes the `--k` window, or gets access-bumped; previously raw BM25 could rank a retired memory above its live successor. `--include-superseded` restores the full-history behavior — the same default-liveness shape `get-edges` has with `--open-only`. Matches `topodb-mcp`'s `search` tool, which already filtered supersession via recall's `tombstone_prop`. `find` is untouched: it is an exact-property lookup, not a recall surface.
- **`search --include-superseded` now reveals forgotten memories too** — the flag is the general history switch over the whole tombstone set (`superseded_at`, `forgotten_at`); default search hides both.
- **Audible retry note on lock contention** — when the database remains locked after 500ms of retrying (under the default 3000ms budget or an explicit `--lock-wait-ms`), a stderr note is printed once: `topodb: database held by another process; retrying (budget <N>ms)`.

### 0.0.8 — 2026-07-23

#### Added

- **`get-edges <from> [--to] [--edge-type] [--open-only <true|false>] [--as-of <unix-ms>]`** — list
  a node's outgoing edges, optionally filtered by target node and/or edge type. `--open-only true`
  (default) shows only open edges; `--open-only false` shows the full history (open + closed).
  `--as-of <unix-ms>` performs a temporal read within the window `valid_from <= t < valid_to`
  (valid_to exclusive) and is mutually exclusive with `--open-only`. Use this to find the edge id
  to pass to `close-edge` when a fact stops being true, or to check what a node is already linked to.
- **`traverse --as-of <unix-ms>`** — temporal graph walk at a Unix millisecond timestamp. Closed
  edges reappear, later edges vanish; a future `as_of` behaves like "now"; omit to read "now".
  **Temporal dimension lives on edges; nodes are always current-state** — `get` readings remain
  timeless, focused on node labels and properties as they exist now.
- **`remember` subcommand** — atomic store-and-link-entities in one call (`--content`, repeatable
  `--entity` [required ≥1], `--edge-type` default `"about"`, repeatable `--supersedes`, `--props`,
  `--scope`). Output: `{"memory_id","deduplicated","entities":[{"name","id","created"}],"edge_ids","superseded"}`.
  Combines memory creation, entity find-or-create, and linking into a single engine batch, so stored
  facts never strand unlinked.
- **`--lock-wait-ms`** / **`TOPODB_LOCK_WAIT_MS` env var** (default 3000, `0` = fail fast) — global
  flag for all subcommands; configures how long to retry on `TopoError::Busy` at startup. Lock
  exhaustion reports `{"error":{"kind":"busy",...}}` and exits with code 3.
- **`--pretty` and `--lock-wait-ms` now valid before or after the subcommand** — these two flags are
  now accepted in any position relative to the subcommand name. `--db` and `--scope` continue to
  require placement before the subcommand (or use `TOPODB_DB` env var for `--db`).

#### Changed

- **`create-memory` and `remember` reject reserved props keys `content_hash`/`superseded_at`** in
  `--props` (exit 2) — these are system-maintained and caller-settable only via dedup and
  supersession primitives, not arbitrary props.
- **Re-storing identical content from a superseded memory creates a new live memory** — `create-memory`
  and `remember` no longer dedup against superseded memories (tombstones). If you submit identical
  wording for a retired fact, it mints a fresh memory; the old tombstone retains its history for
  `as_of`-past queries but never surfaces as a dedup hit.
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
