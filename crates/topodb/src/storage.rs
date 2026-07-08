use crate::counters::AccessStats;
use crate::error::TopoError;
use crate::fts::{doc_text, fts_update};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::index::IndexSpec;
use crate::op::Op;
use crate::state::{EdgeRecord, NodeRecord};
use redb::{Database, ReadableTable, Table, TableDefinition};
use std::path::Path;
use std::sync::Arc;

pub(crate) const OPS: TableDefinition<u64, &[u8]> = TableDefinition::new("ops");
pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub(crate) const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
pub(crate) const EDGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edges");
/// Inverted index: UTF-8 term bytes → postcard `Vec<(NodeId, u32)>` (doc, term
/// frequency), maintained transactionally by `fts_update`. Opened in
/// `open_with`.
pub(crate) const POSTINGS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("postings");
/// Per-document token length: node key → postcard `u32`. Opened in
/// `open_with`.
pub(crate) const FTS_DOCS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fts_docs");
/// Per-scope corpus statistics: `scope_key(scope)` → postcard `(u64, u64)` =
/// `(doc_count, total_len)`. Opened in `open_with`, maintained transactionally
/// by `fts_update`. Replaces the old global `"fts_doc_count"` / `"fts_total_len"`
/// META counters — corpus stats are now sourced per scope so that documents in
/// one scope never shift another scope's BM25 df/avgdl.
pub(crate) const FTS_STATS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fts_stats");
/// Auxiliary per-node access statistics, keyed by the same 16-byte node key as
/// NODES. Deliberately *outside* the op log: never appended to OPS, never
/// broadcast to the change feed, and never touched by `rebuild_state_from_ops`.
pub(crate) const COUNTERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("counters");

pub const FORMAT_VERSION: u32 = 1;

pub struct Storage {
    pub(crate) db: Database,
    /// The index configuration this storage was opened with. Read by
    /// `apply_batch`/`rebuild_state_from_ops`/`ensure_index_spec` (via
    /// `doc_text(&self.spec, ...)`) on every write-path mutation and full
    /// rebuild. Held here (not just threaded through `Snapshot`) precisely so
    /// that write-path access is possible without going through the
    /// in-memory snapshot.
    pub(crate) spec: Arc<IndexSpec>,
}

impl Storage {
    /// Delegates to `open_with` with a default (empty) `IndexSpec` — no
    /// declared indexes. Kept as the parameterless twin of `open_with`
    /// (mirroring `Db::open`/`Db::open_with` one layer up), but it has no
    /// non-test callers: `Db::open` delegates via `Db::open_with`, which
    /// calls `Storage::open_with` directly. Only unit tests call this, hence
    /// the `#[allow(dead_code)]` in non-test builds.
    #[allow(dead_code)]
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, TopoError> {
        Self::open_with(path, Arc::new(IndexSpec::default()))
    }

    pub(crate) fn open_with(path: impl AsRef<Path>, spec: Arc<IndexSpec>) -> Result<Self, TopoError> {
        let db = Database::create(path).map_err(redb::Error::from)?;
        let s = Self { db, spec };
        // Ensure tables + format version exist.
        let tx = s.db.begin_write().map_err(redb::Error::from)?;
        {
            tx.open_table(OPS).map_err(redb::Error::from)?;
            tx.open_table(NODES).map_err(redb::Error::from)?;
            tx.open_table(EDGES).map_err(redb::Error::from)?;
            tx.open_table(COUNTERS).map_err(redb::Error::from)?;
            tx.open_table(POSTINGS).map_err(redb::Error::from)?;
            tx.open_table(FTS_DOCS).map_err(redb::Error::from)?;
            tx.open_table(FTS_STATS).map_err(redb::Error::from)?;
            let mut meta = tx.open_table(META).map_err(redb::Error::from)?;
            // Read the stored version into an owned value first so the read
            // guard's borrow of `meta` ends before we (maybe) insert into it.
            let existing: Option<u32> = match meta.get("format_version").map_err(redb::Error::from)? {
                Some(v) => {
                    let bytes: [u8; 4] = v
                        .value()
                        .try_into()
                        .map_err(|_| TopoError::Encoding("bad format_version".into()))?;
                    Some(u32::from_le_bytes(bytes))
                }
                None => None,
            };
            match existing {
                None => {
                    meta.insert("format_version", FORMAT_VERSION.to_le_bytes().as_slice())
                        .map_err(redb::Error::from)?;
                }
                Some(found) if found > FORMAT_VERSION => {
                    return Err(TopoError::UnsupportedFormat { found, supported: FORMAT_VERSION });
                }
                // Reachable only for `found < FORMAT_VERSION` (the `>` arm
                // above catches the rest) — i.e. a pre-1 file, which no
                // released build ever writes. Kept as a guard against
                // corrupt/hand-rolled files rather than dead logic.
                Some(found) if found != FORMAT_VERSION => {
                    return Err(TopoError::Encoding(format!(
                        "unsupported format version {found}, this build supports {FORMAT_VERSION}"
                    )));
                }
                Some(_) => {}
            }
        }
        tx.commit().map_err(redb::Error::from)?;
        // Reconcile the stored index spec with the one we were opened with;
        // a text-portion change (or a legacy layout) triggers a full reindex
        // (committed here, before any reader/`Db` observes the tables).
        s.ensure_index_spec()?;
        Ok(s)
    }

    /// Reconciles the on-disk text index with the `IndexSpec` this storage was
    /// opened with, and persists the full spec under META `"index_spec"`.
    ///
    /// The stored spec has BOTH its `equality` and `text` lists sorted by
    /// `(label, prop)` before encoding, so a mere reordering of declarations
    /// never looks like a change. Only the **text** portion drives reindexing:
    /// the equality index is rebuilt in memory at every open from NODES (see
    /// `graph.rs`), so a changed equality list needs no storage work — it is
    /// persisted here purely for FORMAT.md introspection/tooling.
    ///
    /// Reindex decision (one write transaction):
    /// - Legacy Plan-2 layout (`"fts_spec"` present): the on-disk postings are
    ///   keyed by bare term (no scope prefix) and corpus stats live in the
    ///   `"fts_doc_count"`/`"fts_total_len"` META counters — incompatible with
    ///   the per-scope layout. Always drain + full reindex, and delete the three
    ///   legacy keys.
    /// - New layout (`"index_spec"` present): reindex iff the stored text list
    ///   differs from the incoming one.
    /// - Plan-1 file (neither key): reindex iff the incoming text list is
    ///   non-empty (nothing was ever indexed).
    ///
    /// A reindex drains POSTINGS/FTS_DOCS/FTS_STATS and rebuilds by scanning
    /// NODES through `fts_update` (threading each node's immutable scope), so
    /// the new postings are scope-prefixed and FTS_STATS is per-scope.
    fn ensure_index_spec(&self) -> Result<(), TopoError> {
        let incoming = normalized_spec(&self.spec);
        let incoming_bytes =
            postcard::to_allocvec(&incoming).map_err(|e| TopoError::Encoding(e.to_string()))?;

        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        let (needs_reindex, is_legacy_v1) = {
            let meta = tx.open_table(META).map_err(redb::Error::from)?;
            if meta.get("fts_spec").map_err(redb::Error::from)?.is_some() {
                (true, true)
            } else {
                match meta.get("index_spec").map_err(redb::Error::from)? {
                    Some(v) => {
                        let stored: IndexSpec = postcard::from_bytes(v.value())
                            .map_err(|e| TopoError::Encoding(e.to_string()))?;
                        (stored.text != incoming.text, false)
                    }
                    None => (!incoming.text.is_empty(), false),
                }
            }
        };

        if needs_reindex {
            let mut postings = tx.open_table(POSTINGS).map_err(redb::Error::from)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(redb::Error::from)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(redb::Error::from)?;
            postings.retain(|_, _| false).map_err(redb::Error::from)?;
            docs.retain(|_, _| false).map_err(redb::Error::from)?;
            stats.retain(|_, _| false).map_err(redb::Error::from)?;

            let nodes = tx.open_table(NODES).map_err(redb::Error::from)?;
            for entry in nodes.iter().map_err(redb::Error::from)? {
                let (_, v) = entry.map_err(redb::Error::from)?;
                let rec: NodeRecord = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                let new_text = doc_text(&self.spec, &rec);
                fts_update(
                    &mut postings,
                    &mut docs,
                    &mut stats,
                    rec.scope,
                    rec.id,
                    None,
                    new_text.as_deref(),
                )?;
            }
        }

        {
            let mut meta = tx.open_table(META).map_err(redb::Error::from)?;
            if is_legacy_v1 {
                meta.remove("fts_spec").map_err(redb::Error::from)?;
                meta.remove("fts_doc_count").map_err(redb::Error::from)?;
                meta.remove("fts_total_len").map_err(redb::Error::from)?;
            }
            // Persist the full normalized spec unconditionally so the stored
            // spec always reflects the current open (a byte-identical rewrite
            // is a harmless no-op). Introspection sees equality changes even
            // when they trigger no reindex.
            meta.insert("index_spec", incoming_bytes.as_slice()).map_err(redb::Error::from)?;
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    /// Reads back the stored `format_version`. `Storage` itself is not part
    /// of the crate's public API (never re-exported from `lib.rs`), so this
    /// `pub` is inert outside the crate; it is exercised only by unit tests
    /// today, hence `#[allow(dead_code)]` in non-test builds — same class as
    /// `append_ops`/`open` above.
    #[allow(dead_code)]
    pub fn format_version(&self) -> Result<u32, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let meta = tx.open_table(META).map_err(redb::Error::from)?;
        let v = meta.get("format_version").map_err(redb::Error::from)?
            .ok_or_else(|| TopoError::Encoding("missing format_version".into()))?;
        let bytes: [u8; 4] = v.value().try_into()
            .map_err(|_| TopoError::Encoding("bad format_version".into()))?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Raw op-log append — bypasses resolution/validation, so it is *not*
    /// part of the write path (`apply_batch` is). Kept `pub(crate)` and
    /// exercised only by unit tests: a low-level seam reserved for the future
    /// compaction/replication layer, never exposed to external consumers.
    #[allow(dead_code)]
    pub(crate) fn append_ops(&self, ops: &[Op]) -> Result<(u64, u64), TopoError> {
        if ops.is_empty() {
            return Err(TopoError::Rejected("empty op batch".into()));
        }
        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        let (first, last);
        {
            let mut table = tx.open_table(OPS).map_err(redb::Error::from)?;
            let next = table.last().map_err(redb::Error::from)?
                .map(|(k, _)| k.value() + 1).unwrap_or(1);
            first = next;
            last = next + ops.len() as u64 - 1;
            for (i, op) in ops.iter().enumerate() {
                let bytes = postcard::to_allocvec(op)
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                table.insert(next + i as u64, bytes.as_slice())
                    .map_err(redb::Error::from)?;
            }
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok((first, last))
    }

    /// The oldest op seq still retained in the log. Sourced from META
    /// `"oldest_seq"` (u64 LE), written only by `compact_ops_through`. An
    /// ABSENT key means the log has never been compacted, so the oldest
    /// retained seq is 1 (the genesis seq).
    pub(crate) fn oldest_seq(&self) -> Result<u64, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let meta = tx.open_table(META).map_err(redb::Error::from)?;
        read_oldest_seq(&meta)
    }

    /// The highest op seq currently in the log (its last OPS key), or 0 when
    /// the log is empty. A plain storage read — no applier round-trip — so it
    /// is safe to call from any thread as the anchor for a live tail
    /// (`current_seq` then `subscribe` then `ops_since(seq + 1)`).
    pub(crate) fn current_seq(&self) -> Result<u64, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(OPS).map_err(redb::Error::from)?;
        let last = table.last().map_err(redb::Error::from)?.map(|(k, _)| k.value()).unwrap_or(0);
        Ok(last)
    }

    /// Drops op-log entries with seq `< keep_from` in one write transaction and
    /// records the new floor under META `"oldest_seq"`. Edge behaviour:
    /// - `keep_from <= oldest_seq`: nothing to trim — no-op `Ok(())` (the write
    ///   txn is aborted, not committed).
    /// - `keep_from > current_seq + 1`: would advance the floor past the log's
    ///   end (skipping never-written seqs) — `TopoError::Rejected`.
    /// - `keep_from == current_seq + 1`: legal; retains an empty tail so the
    ///   next append still lands at `current_seq + 1`.
    ///
    /// Only ever called on the applier thread (the sole redb writer), so the
    /// `oldest_seq`/`current_seq` reads and the delete-and-stamp write are
    /// effectively atomic against other writes.
    pub(crate) fn compact_ops_through(&self, keep_from: u64) -> Result<(), TopoError> {
        let oldest = self.oldest_seq()?;
        if keep_from <= oldest {
            return Ok(());
        }
        let current = self.current_seq()?;
        if keep_from > current + 1 {
            return Err(TopoError::Rejected(format!(
                "compact: keep_from {keep_from} exceeds current_seq {current} + 1"
            )));
        }
        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        {
            let mut ops = tx.open_table(OPS).map_err(redb::Error::from)?;
            ops.retain_in(..keep_from, |_, _| false).map_err(redb::Error::from)?;
            let mut meta = tx.open_table(META).map_err(redb::Error::from)?;
            meta.insert("oldest_seq", keep_from.to_le_bytes().as_slice())
                .map_err(redb::Error::from)?;
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    /// Sequential op-log read from `since` (INCLUSIVE). Backs
    /// `Db::ops_since` — the pull side of the change feed — and is the seam
    /// the compaction layer reads through.
    ///
    /// Returns `TopoError::Compacted { oldest }` when `since < oldest_seq`: the
    /// requested range dips below the retained floor, so the caller must
    /// re-anchor from materialized state rather than receive a silently partial
    /// replay. The `oldest_seq` check and the range read share ONE
    /// `begin_read` transaction, so the returned ops are always consistent with
    /// the floor they were validated against — a concurrent compaction commits
    /// atomically and is either fully visible or not visible to this snapshot.
    pub(crate) fn read_ops(&self, since: u64) -> Result<Vec<(u64, Op)>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let meta = tx.open_table(META).map_err(redb::Error::from)?;
        let oldest = read_oldest_seq(&meta)?;
        if since < oldest {
            return Err(TopoError::Compacted { oldest });
        }
        let table = tx.open_table(OPS).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for entry in table.range(since..).map_err(redb::Error::from)? {
            let (k, v) = entry.map_err(redb::Error::from)?;
            let op: Op = postcard::from_bytes(v.value())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push((k.value(), op));
        }
        Ok(out)
    }

    /// Resolves defaults, validates, appends the resolved ops AND updates the
    /// NODES/EDGES state tables in one redb write transaction. On any
    /// validation failure nothing is committed and `TopoError::Rejected` is
    /// returned.
    pub(crate) fn apply_batch(&self, ops: Vec<Op>, now_ms: i64) -> Result<AppliedBatch, TopoError> {
        if ops.is_empty() {
            return Err(TopoError::Rejected("empty op batch".into()));
        }

        // Resolve defaults up front — the resolved op is what gets appended
        // and applied, so replay stays deterministic.
        let resolved: Vec<Op> = ops.into_iter().map(|op| resolve_op(op, now_ms)).collect();

        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        // Text-index edits collected during the op loop and applied AFTER every
        // op has succeeded — still inside this transaction, so the postings
        // ride the batch's atomicity (a later failing op aborts the whole txn,
        // leaving the index untouched). `old_text` is captured BEFORE `apply_op`
        // mutates the record.
        // Each edit also carries the node's scope (immutable — old and new
        // scope are always identical), needed to key per-scope postings/stats.
        let mut fts_edits: Vec<(Scope, NodeId, Option<String>, Option<String>)> = Vec::new();
        {
            let mut nodes = tx.open_table(NODES).map_err(redb::Error::from)?;
            let mut edges = tx.open_table(EDGES).map_err(redb::Error::from)?;
            for op in &resolved {
                // `pre` carries (id, scope, old_text). For CreateNode the scope
                // comes from the op; for existing-node ops it comes from the
                // record read before mutation.
                let pre: Option<(NodeId, Scope, Option<String>)> = match op {
                    Op::CreateNode { id, scope, .. } => Some((*id, *scope, None)),
                    Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => {
                        match read_node(&nodes, *id)? {
                            Some(rec) => Some((*id, rec.scope, doc_text(&self.spec, &rec))),
                            None => None,
                        }
                    }
                    // SetEmbedding never changes text; edge ops carry none.
                    _ => None,
                };
                apply_op(&mut nodes, &mut edges, op)?;
                if let Some((id, scope, old_text)) = pre {
                    let new_text = match op {
                        Op::RemoveNode { .. } => None,
                        _ => read_node(&nodes, id)?.and_then(|rec| doc_text(&self.spec, &rec)),
                    };
                    fts_edits.push((scope, id, old_text, new_text));
                }
            }
        }
        {
            let mut postings = tx.open_table(POSTINGS).map_err(redb::Error::from)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(redb::Error::from)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(redb::Error::from)?;
            for (scope, id, old_text, new_text) in &fts_edits {
                fts_update(
                    &mut postings,
                    &mut docs,
                    &mut stats,
                    *scope,
                    *id,
                    old_text.as_deref(),
                    new_text.as_deref(),
                )?;
            }
        }

        let (first_seq, last_seq);
        {
            let mut table = tx.open_table(OPS).map_err(redb::Error::from)?;
            let next = table
                .last()
                .map_err(redb::Error::from)?
                .map(|(k, _)| k.value() + 1)
                .unwrap_or(1);
            first_seq = next;
            last_seq = next + resolved.len() as u64 - 1;
            for (i, op) in resolved.iter().enumerate() {
                let bytes =
                    postcard::to_allocvec(op).map_err(|e| TopoError::Encoding(e.to_string()))?;
                table
                    .insert(next + i as u64, bytes.as_slice())
                    .map_err(redb::Error::from)?;
            }
        }

        tx.commit().map_err(redb::Error::from)?;
        Ok(AppliedBatch { first_seq, last_seq, resolved })
    }

    /// Same rationale/`#[allow(dead_code)]` as `format_version` above:
    /// crate-internal only (`Storage` isn't re-exported), exercised only by
    /// unit tests today.
    #[allow(dead_code)]
    pub fn load_node(&self, id: NodeId) -> Result<Option<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(NODES).map_err(redb::Error::from)?;
        read_node(&table, id)
    }

    /// See `load_node`.
    #[allow(dead_code)]
    pub fn load_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(EDGES).map_err(redb::Error::from)?;
        read_edge(&table, id)
    }

    /// Crate-internal full scan — used to rebuild in-memory adjacency. Not
    /// public API: callers should go through the (future) query layer.
    pub(crate) fn all_nodes(&self) -> Result<Vec<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(NODES).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(redb::Error::from)? {
            let (_, v) = entry.map_err(redb::Error::from)?;
            let rec: NodeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push(rec);
        }
        Ok(out)
    }

    pub(crate) fn all_edges(&self) -> Result<Vec<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(EDGES).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(redb::Error::from)? {
            let (_, v) = entry.map_err(redb::Error::from)?;
            let rec: EdgeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push(rec);
        }
        Ok(out)
    }

    /// Folds a batch of access-counter bumps into the COUNTERS table in ONE
    /// write transaction. Each `(id, n, ts)` is applied read-modify-write:
    /// `access_count += n`, `last_accessed_at = max(existing, ts)`. This is the
    /// only writer of COUNTERS and is driven exclusively by the applier thread
    /// via `Job::BumpCounters`, so bumps serialize with (but are recorded
    /// separately from) graph writes. Nothing here appends to OPS or broadcasts
    /// to the change feed — counters live outside the durable log by design.
    pub(crate) fn merge_counter_bumps(
        &self,
        bumps: &[(NodeId, u64, i64)],
    ) -> Result<(), TopoError> {
        if bumps.is_empty() {
            return Ok(());
        }
        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        {
            let mut table = tx.open_table(COUNTERS).map_err(redb::Error::from)?;
            for (id, n, ts) in bumps {
                let key = node_key(*id);
                let existing = match table.get(key.as_slice()).map_err(redb::Error::from)? {
                    Some(v) => postcard::from_bytes::<AccessStats>(v.value())
                        .map_err(|e| TopoError::Encoding(e.to_string()))?,
                    None => AccessStats::default(),
                };
                let merged = AccessStats {
                    access_count: existing.access_count + n,
                    last_accessed_at: existing.last_accessed_at.max(*ts),
                };
                let bytes = postcard::to_allocvec(&merged)
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                table.insert(key.as_slice(), bytes.as_slice()).map_err(redb::Error::from)?;
            }
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok(())
    }

    /// Reads the raw counter row for `id`, or `None` if the node has never been
    /// counted. Scope gating is the caller's responsibility (`Db::access_stats`
    /// checks node existence/scope first); this is a pure COUNTERS lookup.
    pub(crate) fn read_counter(&self, id: NodeId) -> Result<Option<AccessStats>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(COUNTERS).map_err(redb::Error::from)?;
        let key = node_key(id);
        match table.get(key.as_slice()).map_err(redb::Error::from)? {
            None => Ok(None),
            Some(v) => {
                let stats: AccessStats = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                Ok(Some(stats))
            }
        }
    }

    /// Rebuilds NODES/EDGES from scratch by replaying the OPS log in seq
    /// order through the same `apply_op` used by `apply_batch` — no parallel
    /// mutation logic. One write transaction: the state tables are drained
    /// and repopulated atomically, so a reader (or a crash) never observes a
    /// partially-rebuilt graph.
    ///
    /// Validation (endpoint existence, cross-scope rule, missing/duplicate
    /// close, ...) is *not* re-run here: every op in the log already passed
    /// it at append time, and `apply_batch` only ever appends ops it also
    /// applied successfully in the same transaction. `apply_op` still
    /// enforces its own invariants (e.g. `RemoveNode` on a target that
    /// doesn't exist), but replaying a valid log in order cannot hit those
    /// paths; if it does, the log itself is corrupt and surfacing
    /// `TopoError::Rejected` here is the correct, honest outcome.
    ///
    /// The COUNTERS table is intentionally *not* opened or drained here: access
    /// counters are auxiliary telemetry outside the op log, so a state rebuild
    /// must preserve them rather than reset them to zero.
    ///
    /// Refuses with `TopoError::Compacted { oldest }` once `oldest_seq > 1`:
    /// after compaction the log is no longer a full history, so replay from
    /// genesis is impossible by definition. NODES/EDGES remain the materialized
    /// source of truth for a compacted database — there is no full-history
    /// rebuild to fall back on, and none is needed.
    pub(crate) fn rebuild_state_from_ops(&self) -> Result<(), TopoError> {
        let oldest = self.oldest_seq()?;
        if oldest > 1 {
            return Err(TopoError::Compacted { oldest });
        }
        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        {
            let mut nodes = tx.open_table(NODES).map_err(redb::Error::from)?;
            let mut edges = tx.open_table(EDGES).map_err(redb::Error::from)?;
            // The text index is derived from state, so it is drained and rebuilt
            // alongside NODES/EDGES through the very same `fts_update` used on the
            // write path — no parallel maintenance logic.
            let mut postings = tx.open_table(POSTINGS).map_err(redb::Error::from)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(redb::Error::from)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(redb::Error::from)?;
            nodes.retain(|_, _| false).map_err(redb::Error::from)?;
            edges.retain(|_, _| false).map_err(redb::Error::from)?;
            postings.retain(|_, _| false).map_err(redb::Error::from)?;
            docs.retain(|_, _| false).map_err(redb::Error::from)?;
            stats.retain(|_, _| false).map_err(redb::Error::from)?;

            let ops_table = tx.open_table(OPS).map_err(redb::Error::from)?;
            for entry in ops_table.iter().map_err(redb::Error::from)? {
                let (_, v) = entry.map_err(redb::Error::from)?;
                let op: Op = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                // Same (id, scope, old_text, new_text) derivation as
                // `apply_batch`: old_text read BEFORE `apply_op` mutates the
                // record; scope from the op (create) or the pre-mutation record.
                let pre: Option<(NodeId, Scope, Option<String>)> = match &op {
                    Op::CreateNode { id, scope, .. } => Some((*id, *scope, None)),
                    Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => {
                        match read_node(&nodes, *id)? {
                            Some(rec) => Some((*id, rec.scope, doc_text(&self.spec, &rec))),
                            None => None,
                        }
                    }
                    _ => None,
                };
                apply_op(&mut nodes, &mut edges, &op)?;
                if let Some((id, scope, old_text)) = pre {
                    let new_text = match &op {
                        Op::RemoveNode { .. } => None,
                        _ => read_node(&nodes, id)?.and_then(|rec| doc_text(&self.spec, &rec)),
                    };
                    fts_update(
                        &mut postings,
                        &mut docs,
                        &mut stats,
                        scope,
                        id,
                        old_text.as_deref(),
                        new_text.as_deref(),
                    )?;
                }
            }
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct AppliedBatch {
    pub first_seq: u64,
    pub last_seq: u64,
    pub resolved: Vec<Op>,
}

/// Fills `CreateEdge.valid_from` / `CloseEdge.valid_to` with `Some(now_ms)`
/// where the caller left them `None`. All other variants pass through
/// unchanged. Idempotent: an already-resolved op (`Some(_)`) is left as-is.
fn resolve_op(op: Op, now_ms: i64) -> Op {
    match op {
        Op::CreateEdge { id, scope, ty, from, to, props, valid_from } => Op::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from: Some(valid_from.unwrap_or(now_ms)),
        },
        Op::CloseEdge { id, valid_to } => {
            Op::CloseEdge { id, valid_to: Some(valid_to.unwrap_or(now_ms)) }
        }
        other => other,
    }
}

pub(crate) fn node_key(id: NodeId) -> [u8; 16] {
    id.0 .0.to_be_bytes()
}

/// Reads META `"oldest_seq"` (u64 LE) from an already-open META table; an
/// ABSENT key means the log was never compacted, so the floor is 1. Factored
/// out so `oldest_seq` (own read txn) and `read_ops` (shares the read txn with
/// its range scan for a consistent view) derive the floor identically.
fn read_oldest_seq(
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<u64, TopoError> {
    match meta.get("oldest_seq").map_err(redb::Error::from)? {
        Some(v) => {
            let bytes: [u8; 8] = v
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad oldest_seq".into()))?;
            Ok(u64::from_le_bytes(bytes))
        }
        None => Ok(1),
    }
}

/// A copy of `spec` with both index lists sorted by `(label, prop)`, so the
/// on-disk `"index_spec"` encoding is canonical and a declaration reorder is
/// not mistaken for a spec change.
fn normalized_spec(spec: &IndexSpec) -> IndexSpec {
    let key = |p: &crate::index::PropIndex| (p.label.clone(), p.prop.clone());
    let mut equality = spec.equality.clone();
    let mut text = spec.text.clone();
    equality.sort_by_key(&key);
    text.sort_by_key(&key);
    IndexSpec { equality, text }
}

/// Fixed-width 17-byte scope key: a 1-byte tag (`0x00` Shared, `0x01` Id)
/// followed by 16 big-endian ULID bytes (all-zero for Shared). Mirrors
/// `node_key`'s BE-ULID layout. The fixed width is load-bearing: it lets
/// `posting_key` concatenate `scope_key ++ term` with no separator, since no
/// scope prefix can ever be a prefix of another scope's key.
pub(crate) fn scope_key(s: Scope) -> [u8; 17] {
    let mut key = [0u8; 17];
    match s {
        Scope::Shared => {
            key[0] = 0x00;
            // bytes 1..17 stay zero
        }
        Scope::Id(id) => {
            key[0] = 0x01;
            key[1..17].copy_from_slice(&id.0 .0.to_be_bytes());
        }
    }
    key
}

fn edge_key(id: EdgeId) -> [u8; 16] {
    id.0 .0.to_be_bytes()
}

fn read_node(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<Option<NodeRecord>, TopoError> {
    let key = node_key(id);
    match table.get(key.as_slice()).map_err(redb::Error::from)? {
        None => Ok(None),
        Some(v) => {
            let rec: NodeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(rec))
        }
    }
}

fn read_edge(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: EdgeId,
) -> Result<Option<EdgeRecord>, TopoError> {
    let key = edge_key(id);
    match table.get(key.as_slice()).map_err(redb::Error::from)? {
        None => Ok(None),
        Some(v) => {
            let rec: EdgeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(rec))
        }
    }
}

fn put_node(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    rec: &NodeRecord,
) -> Result<(), TopoError> {
    let key = node_key(rec.id);
    let bytes = postcard::to_allocvec(rec).map_err(|e| TopoError::Encoding(e.to_string()))?;
    table.insert(key.as_slice(), bytes.as_slice()).map_err(redb::Error::from)?;
    Ok(())
}

fn put_edge(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    rec: &EdgeRecord,
) -> Result<(), TopoError> {
    let key = edge_key(rec.id);
    let bytes = postcard::to_allocvec(rec).map_err(|e| TopoError::Encoding(e.to_string()))?;
    table.insert(key.as_slice(), bytes.as_slice()).map_err(redb::Error::from)?;
    Ok(())
}

/// Applies a single (already-resolved) op to the NODES/EDGES tables,
/// validating against the current table state — which, mid-batch, already
/// reflects every earlier op in the same batch since we mutate the tables
/// incrementally within the one write transaction. Factored out so Task 7's
/// replay can reuse it without re-deriving the mutation logic.
fn apply_op(
    nodes: &mut Table<'_, &'static [u8], &'static [u8]>,
    edges: &mut Table<'_, &'static [u8], &'static [u8]>,
    op: &Op,
) -> Result<(), TopoError> {
    match op {
        Op::CreateNode { id, scope, label, props } => {
            let rec = NodeRecord {
                id: *id,
                scope: *scope,
                label: label.clone(),
                props: props.clone(),
                embedding: None,
            };
            put_node(nodes, &rec)
        }
        Op::SetNodeProps { id, props } => {
            let mut rec = read_node(nodes, *id)?.ok_or_else(|| {
                TopoError::Rejected(format!("SetNodeProps: node {id:?} not found"))
            })?;
            for (k, v) in props {
                match v {
                    Some(val) => {
                        rec.props.insert(k.clone(), val.clone());
                    }
                    None => {
                        rec.props.remove(k);
                    }
                }
            }
            put_node(nodes, &rec)
        }
        Op::SetEmbedding { id, model, vector } => {
            let mut rec = read_node(nodes, *id)?.ok_or_else(|| {
                TopoError::Rejected(format!("SetEmbedding: node {id:?} not found"))
            })?;
            rec.embedding = Some((model.clone(), vector.clone()));
            put_node(nodes, &rec)
        }
        Op::RemoveNode { id } => {
            let key = node_key(*id);
            let removed = nodes.remove(key.as_slice()).map_err(redb::Error::from)?;
            if removed.is_none() {
                return Err(TopoError::Rejected(format!("RemoveNode: node {id:?} not found")));
            }

            // Remove incident edges, both directions. v0.1: linear scan is
            // acceptable; adjacency-assisted delete arrives with Task 5.
            let mut incident = Vec::new();
            for entry in edges.iter().map_err(redb::Error::from)? {
                let (k, v) = entry.map_err(redb::Error::from)?;
                let rec: EdgeRecord = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                if rec.from == *id || rec.to == *id {
                    incident.push(k.value().to_vec());
                }
            }
            for key in incident {
                edges.remove(key.as_slice()).map_err(redb::Error::from)?;
            }
            Ok(())
        }
        Op::CreateEdge { id, scope, ty, from, to, props, valid_from } => {
            let from_rec = read_node(nodes, *from)?.ok_or_else(|| {
                TopoError::Rejected(format!("CreateEdge {id:?}: from node {from:?} not found"))
            })?;
            let to_rec = read_node(nodes, *to)?.ok_or_else(|| {
                TopoError::Rejected(format!("CreateEdge {id:?}: to node {to:?} not found"))
            })?;
            if from_rec.scope != to_rec.scope
                && from_rec.scope != Scope::Shared
                && to_rec.scope != Scope::Shared
            {
                return Err(TopoError::Rejected(format!(
                    "CreateEdge {id:?}: cross-scope edge requires at least one Shared endpoint"
                )));
            }
            let rec = EdgeRecord {
                id: *id,
                scope: *scope,
                ty: ty.clone(),
                from: *from,
                to: *to,
                props: props.clone(),
                valid_from: valid_from
                    .expect("apply_op only runs on resolved ops (valid_from filled by resolve_op)"),
                valid_to: None,
            };
            put_edge(edges, &rec)
        }
        Op::CloseEdge { id, valid_to } => {
            let mut rec = read_edge(edges, *id)?
                .ok_or_else(|| TopoError::Rejected(format!("CloseEdge: edge {id:?} not found")))?;
            if rec.valid_to.is_some() {
                return Err(TopoError::Rejected(format!("CloseEdge: edge {id:?} already closed")));
            }
            rec.valid_to = Some(
                valid_to
                    .expect("apply_op only runs on resolved ops (valid_to filled by resolve_op)"),
            );
            put_edge(edges, &rec)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::op::Op;

    #[test]
    fn append_assigns_monotonic_seq_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let ops = vec![
            Op::CreateNode { id: NodeId::new(), scope, label: "Memory".into(), props: Default::default() },
            Op::CreateNode { id: NodeId::new(), scope, label: "Entity".into(), props: Default::default() },
        ];
        let (first, last) = s.append_ops(&ops).unwrap();
        assert_eq!((first, last), (1, 2));
        let read = s.read_ops(1).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].1, ops[0]);
        assert_eq!(s.format_version().unwrap(), 1);
    }

    #[test]
    fn open_rejects_unsupported_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        // A freshly-created db opens fine and stamps FORMAT_VERSION.
        drop(Storage::open(&path).unwrap());

        // Corrupt the stored version to an unsupported value via a raw redb
        // write (bypassing `Storage`, which is the whole point).
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut meta = tx.open_table(META).unwrap();
                meta.insert("format_version", 2u32.to_le_bytes().as_slice()).unwrap();
            }
            tx.commit().unwrap();
        }

        // Reopening must now be rejected rather than silently accepted.
        // `.err()` drops the (non-`Debug`) `Storage` from the `Ok` arm.
        let err = Storage::open(&path).err().expect("reopen must be rejected");
        match err {
            TopoError::UnsupportedFormat { found: 2, supported: 1 } => {}
            other => panic!("expected UnsupportedFormat {{ found: 2, supported: 1 }}, got {other:?}"),
        }
    }

    #[test]
    fn set_embedding_lands_in_record_and_rejects_missing_node() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let id = NodeId::new();
        s.apply_batch(
            vec![Op::CreateNode { id, scope, label: "M".into(), props: Default::default() }],
            0,
        )
        .unwrap();
        s.apply_batch(
            vec![Op::SetEmbedding { id, model: "m".into(), vector: vec![1.0, 2.0, 3.0] }],
            0,
        )
        .unwrap();

        let rec = s.load_node(id).unwrap().unwrap();
        assert_eq!(rec.embedding, Some(("m".to_string(), vec![1.0, 2.0, 3.0])));

        // Embedding a node that doesn't exist rejects the whole batch.
        let err = s
            .apply_batch(
                vec![Op::SetEmbedding { id: NodeId::new(), model: "m".into(), vector: vec![0.0] }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, TopoError::Rejected(_)));
    }

    #[test]
    fn append_ops_rejects_empty_batch() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();

        let err = s.append_ops(&[]).unwrap_err();
        assert!(matches!(err, TopoError::Rejected(_)));

        // Nothing was appended.
        assert!(s.read_ops(1).unwrap().is_empty());

        // A subsequent real append still starts at seq 1.
        let ops = vec![Op::CreateNode {
            id: NodeId::new(),
            scope: Scope::Id(ScopeId::new()),
            label: "Memory".into(),
            props: Default::default(),
        }];
        let (first, last) = s.append_ops(&ops).unwrap();
        assert_eq!((first, last), (1, 1));
    }
}
