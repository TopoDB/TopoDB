# TopoDB on-disk format (v1)

TopoDB stores everything in a single [redb](https://github.com/cberner/redb) file. This
document is the durable, public contract for that file's layout: table names, key/value
encodings, and the rules governing how the format may evolve. It is derived directly from
the code — every claim below cites the `storage.rs`/`fts.rs`/etc. **symbol** (function,
struct, const, or enum) it was read from, not a line number, so citations survive
reformatting — and is mechanically checked against a committed fixture
(`crates/topodb/tests/fixtures/v1.redb`, exercised by `crates/topodb/tests/format_fixture.rs`).

If this document and the code ever disagree, **the code wins** — treat the discrepancy as a
bug in this file and fix the doc, not the reader's understanding.

## `format_version`

The current format version is **1**.

```rust
pub const FORMAT_VERSION: u32 = 1;               // storage.rs: const FORMAT_VERSION
```

Stored under `META["format_version"]` (see below). `Storage::open_with` stamps it into a
fresh file and, on an existing file, compares the stored value against the build's
`FORMAT_VERSION`:

- stored `> FORMAT_VERSION` → `TopoError::UnsupportedFormat { found, supported }` — a
  future-format file opened by an older build is rejected outright, never silently
  misread (`storage.rs: fn open_with`).
- stored `< FORMAT_VERSION` (any value other than an exact match, once the `>` arm above
  is excluded) → `TopoError::Encoding` — reachable only for a hand-corrupted or pre-v1
  file, since no released build ever writes anything but the current version
  (`storage.rs: fn open_with`).
- stored `== FORMAT_VERSION` → opens normally.

## Tables

Eight redb tables, all opened unconditionally by `Storage::open_with`
(`storage.rs: fn open_with`). This is the complete table set — there are no others.

| # | Table (redb name string) | `TableDefinition` | Key | Value |
|---|---|---|---|---|
| 1 | `"ops"` | `TableDefinition<u64, &[u8]>` | u64 op sequence number | postcard-encoded [`Op`](#ops-table-value-op) |
| 2 | `"meta"` | `TableDefinition<&str, &[u8]>` | UTF-8 key string | key-specific bytes — see [META keys](#meta-table) |
| 3 | `"nodes"` | `TableDefinition<&[u8], &[u8]>` | 16-byte BE `node_key` | postcard-encoded [`NodeRecord`](#noderecord) |
| 4 | `"edges"` | `TableDefinition<&[u8], &[u8]>` | 16-byte BE `edge_key` | postcard-encoded [`EdgeRecord`](#edgerecord) |
| 5 | `"postings"` | `TableDefinition<&[u8], &[u8]>` | 17-byte `scope_key` ++ UTF-8 term bytes | postcard-encoded `Vec<(NodeId, u32)>`, sorted by `NodeId` |
| 6 | `"fts_docs"` | `TableDefinition<&[u8], &[u8]>` | 16-byte BE `node_key` | postcard-encoded `u32` (document length, in tokens) |
| 7 | `"fts_stats"` | `TableDefinition<&[u8], &[u8]>` | 17-byte `scope_key` | postcard-encoded `(u64, u64)` = `(doc_count, total_len)` |
| 8 | `"counters"` | `TableDefinition<&[u8], &[u8]>` | 16-byte BE `node_key` | postcard-encoded [`AccessStats`](#accessstats) |

```rust
pub(crate) const OPS: TableDefinition<u64, &[u8]> = TableDefinition::new("ops");             // storage.rs: const OPS
pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");           // storage.rs: const META
pub(crate) const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");        // storage.rs: const NODES
pub(crate) const EDGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edges");        // storage.rs: const EDGES
pub(crate) const POSTINGS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("postings");  // storage.rs: const POSTINGS
pub(crate) const FTS_DOCS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fts_docs");  // storage.rs: const FTS_DOCS
pub(crate) const FTS_STATS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fts_stats");// storage.rs: const FTS_STATS
pub(crate) const COUNTERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("counters");  // storage.rs: const COUNTERS
```

Postcard ([`postcard`](https://docs.rs/postcard)) is the value codec used **everywhere** a
value is described above as "postcard-encoded" — there is no table whose value uses a
different serialization scheme.

### Key encodings

- **`node_key(id: NodeId) -> [u8; 16]`**: the node's ULID as 16 big-endian bytes
  (`id.0.0.to_be_bytes()`) — `storage.rs: fn node_key`. Big-endian keys make redb's natural
  byte-order iteration equal to ULID/creation order.
- **`edge_key(id: EdgeId) -> [u8; 16]`**: identical scheme, over `EdgeId`'s ULID —
  `storage.rs: fn edge_key`.
- **`scope_key(s: Scope) -> [u8; 17]`**: a fixed-width 17-byte encoding — 1-byte tag
  (`0x00` = `Scope::Shared`, `0x01` = `Scope::Id`) followed by 16 big-endian ULID bytes
  (all-zero for `Shared`) — `storage.rs: fn scope_key`:

  ```
  byte:   0        1 .. 17
        [ tag ] [ ULID big-endian, or all-zero for Shared ]
  ```

  The fixed width is load-bearing: it lets the postings key concatenate
  `scope_key ++ term` with **no separator**, because no scope's 17-byte prefix can ever
  be a prefix of another scope's 17-byte prefix (`storage.rs: fn scope_key`).
- **Postings key**: `posting_key(scope, term) = scope_key(scope) ++ term.as_bytes()`
  (UTF-8 term bytes, unescaped) — `fts.rs: fn posting_key`.

### `ops` table (value: `Op`)

```rust
pub enum Op {                                                    // op.rs: enum Op
    CreateNode { id: NodeId, scope: Scope, label: SmolStr, props: Props },
    SetNodeProps { id: NodeId, props: BTreeMap<String, Option<PropValue>> },
    SetEmbedding { id: NodeId, model: String, vector: Vec<f32> },
    RemoveNode { id: NodeId },
    CreateEdge { id: EdgeId, scope: Scope, ty: SmolStr, from: NodeId, to: NodeId,
                 props: Props, valid_from: Option<i64> },
    CloseEdge { id: EdgeId, valid_to: Option<i64> },
}
```

Every op appended to the log is **fully resolved** before it is written — timestamps
(`CreateEdge.valid_from`, `CloseEdge.valid_to`) are filled in by `resolve_op` at apply
time, so a stored op never depends on "now" and replay is deterministic (`op.rs: enum Op`
doc comment, `storage.rs: fn resolve_op`).

### `meta` table

`meta`'s key is a `&str`, value is key-specific bytes. Every key the codebase writes or
reads today:

| Key | Value encoding | Written by | Absent means |
|---|---|---|---|
| `"format_version"` | `u32`, little-endian, 4 bytes (`to_le_bytes`) | `Storage::open_with` on first create (`storage.rs: fn open_with`) | never absent after first open |
| `"index_spec"` | postcard-encoded `IndexSpec`, **both `equality` and `text` sorted by `(label, prop)`** | `Storage::ensure_index_spec`, every open (`storage.rs: fn ensure_index_spec`) | pre-index-spec (Plan-1) file |
| `"oldest_seq"` | `u64`, little-endian, 8 bytes (`to_le_bytes`) | `Storage::compact_ops_through` (`storage.rs: fn compact_ops_through`) | log never compacted — treat as `1` |

`"index_spec"`'s sort is what makes the persisted value canonical: reordering the same
declarations in code never looks like a spec change (`storage.rs: fn normalized_spec`, doc
comment at `storage.rs: fn ensure_index_spec`). Only the **text** portion drives a reindex
when it changes; the equality index is rebuilt in memory from `NODES` on every open, so it
is persisted purely for introspection/tooling (`storage.rs: fn ensure_index_spec`).

`"oldest_seq"`'s absence-means-1 convention is shared by both readers of it,
`Storage::oldest_seq` and `Storage::read_ops` (factored into `read_oldest_seq`,
`storage.rs: fn read_oldest_seq`).

Two legacy Plan-2 META keys — `"fts_spec"`, `"fts_doc_count"`, `"fts_total_len"` — are
recognized **on open only**, as the trigger for a one-time full reindex into the current
per-scope layout, and then deleted (`storage.rs: fn ensure_index_spec`). No v1-produced
file ever writes them; they exist in this document only so a maintainer opening an old
file understands why a reindex fires.

### `NodeRecord`

```rust
pub struct NodeRecord {                                    // state.rs: struct NodeRecord
    pub id: NodeId,
    pub scope: Scope,
    pub label: SmolStr,
    pub props: Props,                                       // BTreeMap<String, PropValue>
    pub embedding: Option<(String, Vec<f32>)>,               // (model name, vector)
}
```

### `EdgeRecord`

```rust
pub struct EdgeRecord {                                     // state.rs: struct EdgeRecord
    pub id: EdgeId,
    pub scope: Scope,
    pub ty: SmolStr,
    pub from: NodeId,
    pub to: NodeId,
    pub props: Props,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}
```

### `postings` table (value: `Vec<(NodeId, u32)>`)

One row per `(scope, term)`. The value is the term's postings list within that scope:
`(NodeId, term_frequency)` pairs, **sorted by `NodeId`** — `set_posting` maintains it via
a `BTreeMap<NodeId, u32>` internally and re-encodes it as a sorted `Vec` on every write, so
the on-disk order is deterministic (`fts.rs: fn set_posting`). A node with term-frequency 0
for a term is removed from the list; an empty list removes the row entirely
(`fts.rs: fn set_posting`).

### `fts_docs` table (value: `u32`)

One row per node that currently has non-empty declared text: its token count. Removed
when the node's declared text becomes empty (`fts.rs: fn fts_update`).

### `fts_stats` table (value: `(u64, u64)`)

One row per **scope** that currently has at least one indexed document:
`(doc_count, total_len)` — the corpus size BM25's IDF/`avgdl` are computed from
(`fts.rs: fn read_stats`, `fts.rs: fn write_stats`). Keying this per scope (rather than one
global pair, as in the superseded Plan-2 `"fts_doc_count"`/`"fts_total_len"` META counters)
is what makes BM25 scores scope-isolated: indexing a document in scope B never shifts scope
A's `df`/`avgdl` (`storage.rs: const FTS_STATS`, `fts.rs` module doc). Row dropped when a
scope's last document is removed (`fts.rs: fn write_stats`).

### `counters` table (value: `AccessStats`)

```rust
pub struct AccessStats {                                    // counters.rs: struct AccessStats
    pub access_count: u64,
    pub last_accessed_at: i64,     // milliseconds since Unix epoch
}
```

**`counters` is deliberately outside the durable op log.** It is never appended to `OPS`,
never broadcast on the change feed, and never touched by `rebuild_state_from_ops`
(`storage.rs: const COUNTERS`, `storage.rs: fn rebuild_state_from_ops`; module doc at
`counters.rs` module doc). A bump is folded read-modify-write by the applier thread from a
best-effort, fire-and-forget channel (`storage.rs: fn merge_counter_bumps`,
`db.rs: Job::BumpCounters`). The practical consequence: **deleting (or losing) the
`counters` table loses only access statistics** — recency/hit-count telemetry a host may
use for decay heuristics — never graph state, and never anything replayable from `OPS`. A
`RemoveNode` may even leave an orphaned counter row behind for a since-deleted node; this
is considered benign, since reads of stats gate on current node existence
(`counters.rs` module doc).

## Compaction contract

The op log supports **prefix compaction**: dropping all entries below a floor while
keeping `NODES`/`EDGES` (the materialized state) as the durable source of truth.

- **`oldest_seq`** (`Storage::oldest_seq`, `storage.rs: fn oldest_seq`): the oldest op
  sequence number still retained. Sourced from `META["oldest_seq"]`; absent means the log
  has never been compacted, so the floor is `1`.
- **`Storage::compact_ops_through(keep_from)`** (`storage.rs: fn compact_ops_through`) drops
  every `OPS` entry with seq `< keep_from` and stamps the new floor into
  `META["oldest_seq"]`, in one write transaction. `keep_from <= oldest_seq` is a no-op;
  `keep_from > current_seq + 1` is rejected (`TopoError::Rejected`) as skipping
  never-written seqs; `keep_from == current_seq + 1` is legal and empties the log entirely.
- **`Compacted { oldest }`** (`error.rs: TopoError::Compacted`) is returned by two call
  sites once the requested range dips below the retained floor:
  - `Storage::read_ops(since)` (backing `Db::ops_since`) when `since < oldest_seq`
    (`storage.rs: fn read_ops`) — the caller must re-anchor from materialized state rather
    than receive a silently partial replay. `since` is clamped to `max(1)` before this
    check, so `since == 0` is equivalent to `since == 1` (replay everything) rather than
    always reading as "below the floor."
  - `Storage::rebuild_state_from_ops` (backing the `#[doc(hidden)]`
    `Db::rebuild_state_from_ops`) refuses outright once `oldest_seq > 1`
    (`storage.rs: fn rebuild_state_from_ops`): after compaction the log is no longer a full
    history, so a from-genesis replay is impossible by definition, and `NODES`/`EDGES`
    remain the source of truth with no full-history fallback.
- **Floor clamp at both append sites.** Emptying the log via compaction
  (`keep_from == current_seq + 1`) leaves no sentinel key in `OPS`, so the seq
  high-water mark survives *only* in `META["oldest_seq"]`. Both append paths —
  `Storage::append_ops` (`storage.rs: fn append_ops`) and `Storage::apply_batch`
  (`storage.rs: fn apply_batch`) — read that floor **inside the same write transaction** as
  the append and clamp `next_seq` up to it (`.max(floor)`), which is what keeps seqs
  monotonic across an emptying compaction instead of restarting at `1`.

## Additive-evolution policy

This format is versioned by the single `u32` in `META["format_version"]`
(`storage.rs: const FORMAT_VERSION`, checked at `storage.rs: fn open_with`). The policy for
changing it:

- **Minor, no version bump**: adding a new table, or a new `META` key, that older readers
  simply never open/read. (`Storage::open_with` unconditionally opens all eight current
  tables; a reader from a version that predates a new table would need its own code change
  to look at it, but does not choke on its *presence* — redb tables are opened by name on
  demand, not enumerated.)
- **On-disk serde enums (`Op`, `PropValue`, `Scope`) are APPEND-ONLY — never reordered, never
  inserted in the middle.** postcard encodes an enum's discriminant *positionally* (the Nth
  declared variant serializes as varint `N`), so inserting a new variant anywhere but the
  end, or reordering existing variants, silently renumbers every later variant: an old build
  and a new build then disagree about what a given discriminant means, with **no error at
  all** — this is corruption, not a loud failure, and it **requires a `FORMAT_VERSION`
  bump**, not a minor change.
  A variant appended strictly at the end needs no version bump, but is **forward-incompatible**:
  a build that predates the new variant has no arm for its discriminant and fails loudly with
  `TopoError::Encoding` the moment it decodes a log entry containing it — and there is no
  version signal warning that build this is coming, since `FORMAT_VERSION` did not change.
  Treat an appended variant as a **documented**, not silent, compatibility break: call it out
  here (or in the crate's changelog) so operators know that rolling an older build back onto
  a log a newer build has written can fail outright, and bump `FORMAT_VERSION` instead of
  appending if forward-readability by older builds must be preserved for the release.
- **Format-version bump required**: any change to the **encoding** of an existing table's key
  or value, or of an existing `META` key's value (e.g. changing `node_key` from big-endian to
  little-endian, changing `postings`' value from a `Vec` to a `HashMap`, changing
  `AccessStats`'s field types), or a reordered/inserted (non-append) enum variant as above —
  anything that would make an old build misinterpret bytes written by a new build, or vice
  versa. `FORMAT_VERSION` must be bumped and `Storage::open_with`'s version-compat handling
  (`storage.rs: fn open_with`) extended if any migration path is intended; the current code
  only accepts an exact-match version and rejects anything newer (`UnsupportedFormat`).
- This document (`FORMAT.md`) and the committed fixture
  (`crates/topodb/tests/fixtures/v1.redb`, see below) must be updated together with any
  version bump.

## The committed v1 fixture

`crates/topodb/tests/fixtures/v1.redb` is a small, committed redb file pinning this
layout, generated and read by `crates/topodb/tests/format_fixture.rs`:

- `regenerate_fixture` (`#[ignore]`) creates it fresh: declares an `IndexSpec` with one
  equality index (`Entity.name`) and one text index (`Memory.content`), creates two nodes
  (`NodeId::from_u128(10)` labeled `Entity` with `name = "ada"`, and
  `NodeId::from_u128(11)` labeled `Memory` with `content = "fixture memory about
  databases"`, both in `ScopeId::from_u128(1)`), then sets an embedding
  (`model = "m1"`, `vector = [1.0, 0.0]`) on the second node. Regenerate it explicitly
  with `cargo test -p topodb --test format_fixture -- --ignored regenerate` whenever the
  v1 layout intentionally changes.
- `v1_fixture_opens_and_reads` runs in the normal test suite: it copies the committed
  file to a tempdir (the committed file is **never** opened read-write in place, since
  `open_with` can write to `META` on open) and asserts `nodes_by_prop`, `search_text`,
  `search_vector`, and `current_seq() == 3` all see the expected fixture content.
- `NodeId`/`ScopeId` expose `#[doc(hidden)] fn from_u128(u128) -> Self` (added
  alongside this fixture, `ids.rs`) purely so fixture ids are reproducible in *content*
  across regenerations — the raw `.redb` bytes are not guaranteed byte-identical
  (redb's allocator/free-space layout is not part of this contract), only the documented
  query results are.
- `*.redb` is covered by the repo-wide `.gitignore` (`.gitignore:2`); this one file is
  force-added (`git add -f`) as a deliberate, intentional exception.
