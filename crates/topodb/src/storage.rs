use crate::counters::AccessStats;
use crate::dict::{Dicts, DICT};
use crate::error::{storage_err, TopoError};
use crate::fts::{doc_text, fts_update};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::index::IndexSpec;
use crate::op::Op;
use crate::state::{EdgeRecord, NodeRecord};
use redb::{Database, ReadableTable, Table, TableDefinition};
use std::path::Path;
use std::sync::{Arc, RwLock};

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
/// Cold vector rows: node key -> framed postcard (model, vector).
pub(crate) const EMBEDDINGS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("embeddings");

pub const FORMAT_VERSION: u32 = 2;

/// Stable logical table-byte measurement (redb page and free-list overhead excluded).
#[derive(Debug, Clone)]
pub struct TableReport {
    pub table: &'static str,
    pub rows: u64,
    pub key_bytes: u64,
    pub value_bytes: u64,
}

pub struct Storage {
    pub(crate) db: Database,
    /// The index configuration this storage was opened with. Read by
    /// `apply_batch`/`rebuild_state_from_ops`/`ensure_index_spec` (via
    /// `doc_text(&self.spec, ...)`) on every write-path mutation and full
    /// rebuild. Held here (not just threaded through `Snapshot`) precisely so
    /// that write-path access is possible without going through the
    /// in-memory snapshot.
    pub(crate) spec: Arc<IndexSpec>,
    pub(crate) dicts: RwLock<Dicts>,
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

    pub(crate) fn open_with(
        path: impl AsRef<Path>,
        spec: Arc<IndexSpec>,
    ) -> Result<Self, TopoError> {
        let db = Database::create(path).map_err(storage_err)?;
        let s = Self {
            db,
            spec,
            dicts: RwLock::new(Dicts::default()),
        };
        let tx = s.db.begin_write().map_err(storage_err)?;
        let existing = {
            tx.open_table(OPS).map_err(storage_err)?;
            tx.open_table(NODES).map_err(storage_err)?;
            tx.open_table(EDGES).map_err(storage_err)?;
            tx.open_table(COUNTERS).map_err(storage_err)?;
            tx.open_table(POSTINGS).map_err(storage_err)?;
            tx.open_table(FTS_DOCS).map_err(storage_err)?;
            tx.open_table(FTS_STATS).map_err(storage_err)?;
            tx.open_table(EMBEDDINGS).map_err(storage_err)?;
            tx.open_table(DICT).map_err(storage_err)?;
            let meta = tx.open_table(META).map_err(storage_err)?;
            let version = match meta.get("format_version").map_err(storage_err)? {
                Some(v) => {
                    let b: [u8; 4] = v
                        .value()
                        .try_into()
                        .map_err(|_| TopoError::Encoding("bad format_version".into()))?;
                    Some(u32::from_le_bytes(b))
                }
                None => None,
            };
            version
        };
        match existing {
            None => {
                let mut meta = tx.open_table(META).map_err(storage_err)?;
                meta.insert("format_version", FORMAT_VERSION.to_le_bytes().as_slice())
                    .map_err(storage_err)?;
            }
            Some(2) => {}
            Some(1) => {
                let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
                let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
                let mut emb = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
                let mut dict = tx.open_table(DICT).map_err(storage_err)?;
                let mut d = Dicts::default();
                crate::migrate::migrate_v1_to_v2(
                    &mut nodes, &mut edges, &mut emb, &mut dict, &mut d,
                )?;
                drop(nodes);
                drop(edges);
                drop(emb);
                drop(dict);
                let mut meta = tx.open_table(META).map_err(storage_err)?;
                meta.insert("format_version", FORMAT_VERSION.to_le_bytes().as_slice())
                    .map_err(storage_err)?;
            }
            Some(found) if found > FORMAT_VERSION => {
                return Err(TopoError::UnsupportedFormat {
                    found,
                    supported: FORMAT_VERSION,
                })
            }
            Some(found) => {
                return Err(TopoError::Encoding(format!(
                    "unsupported format version {found}"
                )))
            }
        }
        tx.commit().map_err(storage_err)?;
        let r = s.db.begin_read().map_err(storage_err)?;
        *s.dicts.write().expect("dict lock poisoned") = Dicts::load(&r)?;
        drop(r);
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

        let tx = self.db.begin_write().map_err(storage_err)?;
        let (needs_reindex, is_legacy_v1) = {
            let meta = tx.open_table(META).map_err(storage_err)?;
            if meta.get("fts_spec").map_err(storage_err)?.is_some() {
                (true, true)
            } else {
                match meta.get("index_spec").map_err(storage_err)? {
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
            let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
            postings.retain(|_, _| false).map_err(storage_err)?;
            docs.retain(|_, _| false).map_err(storage_err)?;
            stats.retain(|_, _| false).map_err(storage_err)?;

            let nodes = tx.open_table(NODES).map_err(storage_err)?;
            for entry in nodes.iter().map_err(storage_err)? {
                let (key_guard, _) = entry.map_err(storage_err)?;
                let dicts = self.dicts.read().expect("dict lock poisoned");
                let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
                let key: [u8; 16] = key_guard
                    .value()
                    .try_into()
                    .map_err(|_| TopoError::Encoding("bad node key".into()))?;
                let id = NodeId::from_u128(u128::from_be_bytes(key));
                let rec = read_node(&nodes, &embeddings, &dicts, id)?
                    .ok_or_else(|| TopoError::Encoding("missing node during reindex".into()))?;
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
            let mut meta = tx.open_table(META).map_err(storage_err)?;
            if is_legacy_v1 {
                meta.remove("fts_spec").map_err(storage_err)?;
                meta.remove("fts_doc_count").map_err(storage_err)?;
                meta.remove("fts_total_len").map_err(storage_err)?;
            }
            // Persist the full normalized spec unconditionally so the stored
            // spec always reflects the current open (a byte-identical rewrite
            // is a harmless no-op). Introspection sees equality changes even
            // when they trigger no reindex.
            meta.insert("index_spec", incoming_bytes.as_slice())
                .map_err(storage_err)?;
        }
        tx.commit().map_err(storage_err)?;
        Ok(())
    }

    /// Per-table logical byte counts, used by the reproducible storage benchmark.
    pub fn storage_report(&self) -> Result<Vec<TableReport>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        fn bytes(
            tx: &redb::ReadTransaction,
            table: TableDefinition<&[u8], &[u8]>,
            name: &'static str,
        ) -> Result<TableReport, TopoError> {
            let table = tx.open_table(table).map_err(storage_err)?;
            let mut out = TableReport {
                table: name,
                rows: 0,
                key_bytes: 0,
                value_bytes: 0,
            };
            for entry in table.iter().map_err(storage_err)? {
                let (key, value) = entry.map_err(storage_err)?;
                out.rows += 1;
                out.key_bytes += key.value().len() as u64;
                out.value_bytes += value.value().len() as u64;
            }
            Ok(out)
        }
        let mut out = vec![
            bytes(&tx, NODES, "nodes")?,
            bytes(&tx, EDGES, "edges")?,
            bytes(&tx, EMBEDDINGS, "embeddings")?,
            bytes(&tx, POSTINGS, "postings")?,
            bytes(&tx, FTS_DOCS, "fts_docs")?,
            bytes(&tx, FTS_STATS, "fts_stats")?,
            bytes(&tx, COUNTERS, "counters")?,
        ];
        let dict = tx.open_table(DICT).map_err(storage_err)?;
        let mut dict_report = TableReport {
            table: "dict",
            rows: 0,
            key_bytes: 0,
            value_bytes: 0,
        };
        for entry in dict.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            dict_report.rows += 1;
            dict_report.key_bytes += k.value().len() as u64;
            dict_report.value_bytes += v.value().len() as u64;
        }
        out.push(dict_report);
        let ops = tx.open_table(OPS).map_err(storage_err)?;
        let mut ops_report = TableReport {
            table: "ops",
            rows: 0,
            key_bytes: 0,
            value_bytes: 0,
        };
        for entry in ops.iter().map_err(storage_err)? {
            let (_, v) = entry.map_err(storage_err)?;
            ops_report.rows += 1;
            ops_report.key_bytes += 8;
            ops_report.value_bytes += v.value().len() as u64;
        }
        out.push(ops_report);
        let meta = tx.open_table(META).map_err(storage_err)?;
        let mut meta_report = TableReport {
            table: "meta",
            rows: 0,
            key_bytes: 0,
            value_bytes: 0,
        };
        for entry in meta.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            meta_report.rows += 1;
            meta_report.key_bytes += k.value().len() as u64;
            meta_report.value_bytes += v.value().len() as u64;
        }
        out.push(meta_report);
        Ok(out)
    }

    /// Peeks the `IndexSpec` persisted under META `"index_spec"` by a prior
    /// `ensure_index_spec`, without going through the normal `open_with`
    /// construction (no table-existence writes, no reindex reconciliation) —
    /// a short, read-only look used by `Db::open_stored` to discover what
    /// spec to reopen with. `Ok(None)` covers both "file doesn't exist yet"
    /// (a fresh `Database::create` has no `META` rows) and "file predates
    /// spec persistence" (no `"index_spec"` key) — in both cases the caller
    /// falls back to `IndexSpec::default()`.
    pub(crate) fn read_persisted_index_spec(path: &Path) -> Result<Option<IndexSpec>, TopoError> {
        let db = Database::create(path).map_err(storage_err)?;
        let tx = db.begin_read().map_err(storage_err)?;
        let meta = match tx.open_table(META) {
            Ok(t) => t,
            // A brand-new file has no tables at all yet.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(storage_err(e)),
        };
        match meta.get("index_spec").map_err(storage_err)? {
            None => Ok(None),
            Some(v) => {
                let spec: IndexSpec = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                Ok(Some(spec))
            }
        }
    }

    /// Reads back the stored `format_version`. `Storage` itself is not part
    /// of the crate's public API (never re-exported from `lib.rs`), so this
    /// `pub` is inert outside the crate; called by `Db::format_version` (and
    /// exercised directly by unit tests).
    pub fn format_version(&self) -> Result<u32, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let meta = tx.open_table(META).map_err(storage_err)?;
        let v = meta
            .get("format_version")
            .map_err(storage_err)?
            .ok_or_else(|| TopoError::Encoding("missing format_version".into()))?;
        let bytes: [u8; 4] = v
            .value()
            .try_into()
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
        let tx = self.db.begin_write().map_err(storage_err)?;
        let (first, last);
        {
            // Floor read inside the SAME write txn as the append: after an
            // empty-log compaction only META `"oldest_seq"` carries the seq
            // high-water mark (`retain_in` leaves no sentinel key), so the
            // next seq is one past the last OPS key, clamped up to the floor.
            let floor = {
                let meta = tx.open_table(META).map_err(storage_err)?;
                read_oldest_seq(&meta)?
            };
            let mut table = tx.open_table(OPS).map_err(storage_err)?;
            let next = table
                .last()
                .map_err(storage_err)?
                .map(|(k, _)| k.value() + 1)
                .unwrap_or(1)
                .max(floor);
            first = next;
            last = next + ops.len() as u64 - 1;
            for (i, op) in ops.iter().enumerate() {
                let bytes =
                    postcard::to_allocvec(op).map_err(|e| TopoError::Encoding(e.to_string()))?;
                table
                    .insert(next + i as u64, bytes.as_slice())
                    .map_err(storage_err)?;
            }
        }
        tx.commit().map_err(storage_err)?;
        Ok((first, last))
    }

    /// The oldest op seq still retained in the log. Sourced from META
    /// `"oldest_seq"` (u64 LE), written only by `compact_ops_through`. An
    /// ABSENT key means the log has never been compacted, so the oldest
    /// retained seq is 1 (the genesis seq).
    pub(crate) fn oldest_seq(&self) -> Result<u64, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let meta = tx.open_table(META).map_err(storage_err)?;
        read_oldest_seq(&meta)
    }

    /// The highest op seq the log has ever assigned: `max(last OPS key, 0)`
    /// on a never-compacted log, or `max(last OPS key, oldest_seq - 1)` once
    /// compaction has run. A plain storage read — no applier round-trip — so
    /// it is safe to call from any thread as the anchor for a live tail
    /// (`current_seq` then `subscribe` then `ops_since(seq + 1)`).
    ///
    /// This survives an empty-but-compacted log: `retain_in` leaves no
    /// sentinel OPS key behind, so on its own `last OPS key` would regress to
    /// 0 (or the prior high-water mark, if any keys remain below the new
    /// floor — which never happens post-compaction). Falling back to
    /// `oldest_seq - 1` recovers the true high-water mark from META in that
    /// case, so `ops_since(current_seq() + 1)` never spuriously observes
    /// `Compacted` right after an emptying compaction — the anchor recipe on
    /// [`subscribe`](crate::db::Db::subscribe) is gap-free with no special
    /// casing. On a never-written log (`oldest_seq` absent ⇒ 1), this is
    /// `max(0, 1 - 1) == 0`, unchanged from before.
    pub(crate) fn current_seq(&self) -> Result<u64, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(OPS).map_err(storage_err)?;
        let last = table
            .last()
            .map_err(storage_err)?
            .map(|(k, _)| k.value())
            .unwrap_or(0);
        let meta = tx.open_table(META).map_err(storage_err)?;
        let oldest = read_oldest_seq(&meta)?;
        Ok(last.max(oldest.saturating_sub(1)))
    }

    /// Drops op-log entries with seq `< keep_from` in one write transaction and
    /// records the new floor under META `"oldest_seq"`. Edge behaviour:
    /// - `keep_from <= oldest_seq`: nothing to trim — no-op `Ok(())`, returned
    ///   before any write transaction is begun (there is no txn to abort).
    /// - `keep_from > current_seq + 1`: would advance the floor past the log's
    ///   end (skipping never-written seqs) — `TopoError::Rejected`.
    /// - `keep_from == current_seq + 1`: legal; empties the log entirely.
    ///   `retain_in` leaves no sentinel key behind, so after an empty-log
    ///   compaction the seq high-water mark survives ONLY in META
    ///   `"oldest_seq"` — the append paths (`apply_batch`/`append_ops`)
    ///   consult that floor at append time and clamp the next seq up to it,
    ///   which is what keeps seqs monotonic across an emptying compaction.
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
        let tx = self.db.begin_write().map_err(storage_err)?;
        {
            let mut ops = tx.open_table(OPS).map_err(storage_err)?;
            ops.retain_in(..keep_from, |_, _| false)
                .map_err(storage_err)?;
            let mut meta = tx.open_table(META).map_err(storage_err)?;
            meta.insert("oldest_seq", keep_from.to_le_bytes().as_slice())
                .map_err(storage_err)?;
        }
        tx.commit().map_err(storage_err)?;
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
        // Clamp BEFORE the floor check: seq 0 never exists (seqs start at 1),
        // so `since == 0` must mean "replay everything", exactly like `since
        // == 1` on a never-compacted log. Without this clamp, the default
        // floor of 1 (an uncompacted log) makes `0 < oldest` true and
        // `ops_since(0)` falsely returns `Compacted { oldest: 1 }` on a log
        // that was never compacted at all — breaking the natural
        // "replay-everything" idiom for callers who don't have a real anchor
        // yet.
        let since = since.max(1);
        let tx = self.db.begin_read().map_err(storage_err)?;
        let meta = tx.open_table(META).map_err(storage_err)?;
        let oldest = read_oldest_seq(&meta)?;
        if since < oldest {
            return Err(TopoError::Compacted { oldest });
        }
        let table = tx.open_table(OPS).map_err(storage_err)?;
        let mut out = Vec::new();
        for entry in table.range(since..).map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let op: Op =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
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

        let mut dicts = self.dicts.write().expect("dict lock poisoned");
        // A failed prior write transaction may have interned only in memory.
        // Refresh from committed rows before each write so aborted batches cannot
        // leave phantom ids that a later batch would reference.
        let dict_read = self.db.begin_read().map_err(storage_err)?;
        *dicts = Dicts::load(&dict_read)?;
        drop(dict_read);
        let tx = self.db.begin_write().map_err(storage_err)?;
        // Text-index edits collected during the op loop and applied AFTER every
        // op has succeeded — still inside this transaction, so the postings
        // ride the batch's atomicity (a later failing op aborts the whole txn,
        // leaving the index untouched). `old_text` is captured BEFORE `apply_op`
        // mutates the record.
        // Each edit also carries the node's scope (immutable — old and new
        // scope are always identical), needed to key per-scope postings/stats.
        let mut fts_edits: Vec<(Scope, NodeId, Option<String>, Option<String>)> = Vec::new();
        {
            let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
            let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
            let mut embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
            let mut dict_table = tx.open_table(DICT).map_err(storage_err)?;
            for op in &resolved {
                // `pre` carries (id, scope, old_text). For CreateNode the scope
                // comes from the op; for existing-node ops it comes from the
                // record read before mutation.
                let pre: Option<(NodeId, Scope, Option<String>)> = match op {
                    Op::CreateNode { id, scope, .. } => Some((*id, *scope, None)),
                    Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => {
                        match read_node(&nodes, &embeddings, &dicts, *id)? {
                            Some(rec) => Some((*id, rec.scope, doc_text(&self.spec, &rec))),
                            None => None,
                        }
                    }
                    // SetEmbedding never changes text; edge ops carry none.
                    _ => None,
                };
                apply_op(
                    &mut nodes,
                    &mut edges,
                    &mut embeddings,
                    &mut dict_table,
                    &mut dicts,
                    op,
                )?;
                if let Some((id, scope, old_text)) = pre {
                    let new_text = match op {
                        Op::RemoveNode { .. } => None,
                        _ => read_node(&nodes, &embeddings, &dicts, id)?
                            .and_then(|rec| doc_text(&self.spec, &rec)),
                    };
                    fts_edits.push((scope, id, old_text, new_text));
                }
            }
        }
        {
            let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
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
            // Same floor clamp as `append_ops`, same rationale: after an
            // empty-log compaction the seq high-water mark lives only in META
            // `"oldest_seq"` — deriving `next` from `OPS.last()` alone would
            // restart at 1, committing ops BELOW the floor (permanently
            // unreadable via `read_ops` and breaking seq monotonicity). Read
            // inside this write txn so the clamp is atomic with the append.
            let floor = {
                let meta = tx.open_table(META).map_err(storage_err)?;
                read_oldest_seq(&meta)?
            };
            let mut table = tx.open_table(OPS).map_err(storage_err)?;
            let next = table
                .last()
                .map_err(storage_err)?
                .map(|(k, _)| k.value() + 1)
                .unwrap_or(1)
                .max(floor);
            first_seq = next;
            last_seq = next + resolved.len() as u64 - 1;
            for (i, op) in resolved.iter().enumerate() {
                let bytes =
                    postcard::to_allocvec(op).map_err(|e| TopoError::Encoding(e.to_string()))?;
                table
                    .insert(next + i as u64, bytes.as_slice())
                    .map_err(storage_err)?;
            }
        }

        tx.commit().map_err(storage_err)?;
        Ok(AppliedBatch {
            first_seq,
            last_seq,
            resolved,
        })
    }

    /// Same rationale/`#[allow(dead_code)]` as `format_version` above:
    /// crate-internal only (`Storage` isn't re-exported), exercised only by
    /// unit tests today.
    #[allow(dead_code)]
    pub fn load_node(&self, id: NodeId) -> Result<Option<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        read_node(&table, &embeddings, &dicts, id)
    }

    /// See `load_node`.
    #[allow(dead_code)]
    pub fn load_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(EDGES).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        read_edge(&table, &dicts, id)
    }

    /// Crate-internal full scan — used to rebuild in-memory adjacency. Not
    /// public API: callers should go through the (future) query layer.
    pub(crate) fn all_nodes(&self) -> Result<Vec<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let mut out = Vec::new();
        for entry in table.iter().map_err(storage_err)? {
            let (k, _) = entry.map_err(storage_err)?;
            let key: [u8; 16] = k
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad node key".into()))?;
            if let Some(rec) = read_node(
                &table,
                &embeddings,
                &dicts,
                NodeId::from_u128(u128::from_be_bytes(key)),
            )? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    pub(crate) fn all_edges(&self) -> Result<Vec<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(EDGES).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let mut out = Vec::new();
        for entry in table.iter().map_err(storage_err)? {
            let (k, _) = entry.map_err(storage_err)?;
            let key: [u8; 16] = k
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad edge key".into()))?;
            if let Some(rec) =
                read_edge(&table, &dicts, EdgeId::from_u128(u128::from_be_bytes(key)))?
            {
                out.push(rec);
            }
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
        let tx = self.db.begin_write().map_err(storage_err)?;
        {
            let mut table = tx.open_table(COUNTERS).map_err(storage_err)?;
            for (id, n, ts) in bumps {
                let key = node_key(*id);
                let existing = match table.get(key.as_slice()).map_err(storage_err)? {
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
                table
                    .insert(key.as_slice(), bytes.as_slice())
                    .map_err(storage_err)?;
            }
        }
        tx.commit().map_err(storage_err)?;
        Ok(())
    }

    /// Reads the raw counter row for `id`, or `None` if the node has never been
    /// counted. Scope gating is the caller's responsibility (`Db::access_stats`
    /// checks node existence/scope first); this is a pure COUNTERS lookup.
    pub(crate) fn read_counter(&self, id: NodeId) -> Result<Option<AccessStats>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(COUNTERS).map_err(storage_err)?;
        let key = node_key(id);
        match table.get(key.as_slice()).map_err(storage_err)? {
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
        let mut dicts = self.dicts.write().expect("dict lock poisoned");
        let tx = self.db.begin_write().map_err(storage_err)?;
        {
            let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
            let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
            let mut embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
            let mut dict_table = tx.open_table(DICT).map_err(storage_err)?;
            // The text index is derived from state, so it is drained and rebuilt
            // alongside NODES/EDGES through the very same `fts_update` used on the
            // write path — no parallel maintenance logic.
            let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
            nodes.retain(|_, _| false).map_err(storage_err)?;
            edges.retain(|_, _| false).map_err(storage_err)?;
            embeddings.retain(|_, _| false).map_err(storage_err)?;
            dict_table.retain(|_, _| false).map_err(storage_err)?;
            dicts.clear();
            postings.retain(|_, _| false).map_err(storage_err)?;
            docs.retain(|_, _| false).map_err(storage_err)?;
            stats.retain(|_, _| false).map_err(storage_err)?;

            let ops_table = tx.open_table(OPS).map_err(storage_err)?;
            for entry in ops_table.iter().map_err(storage_err)? {
                let (_, v) = entry.map_err(storage_err)?;
                let op: Op = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                // Same (id, scope, old_text, new_text) derivation as
                // `apply_batch`: old_text read BEFORE `apply_op` mutates the
                // record; scope from the op (create) or the pre-mutation record.
                let pre: Option<(NodeId, Scope, Option<String>)> = match &op {
                    Op::CreateNode { id, scope, .. } => Some((*id, *scope, None)),
                    Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => {
                        match read_node(&nodes, &embeddings, &dicts, *id)? {
                            Some(rec) => Some((*id, rec.scope, doc_text(&self.spec, &rec))),
                            None => None,
                        }
                    }
                    _ => None,
                };
                apply_op(
                    &mut nodes,
                    &mut edges,
                    &mut embeddings,
                    &mut dict_table,
                    &mut dicts,
                    &op,
                )?;
                if let Some((id, scope, old_text)) = pre {
                    let new_text = match &op {
                        Op::RemoveNode { .. } => None,
                        _ => read_node(&nodes, &embeddings, &dicts, id)?
                            .and_then(|rec| doc_text(&self.spec, &rec)),
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
        tx.commit().map_err(storage_err)?;
        Ok(())
    }
}

/// The committed result of a successful [`crate::Db::submit`]/
/// [`crate::Db::submit_at`] call: the inclusive `[first_seq, last_seq]` range
/// the batch's ops were assigned in the durable op log, and the batch's ops
/// in their fully-resolved form (timestamps filled in) as actually written.
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
        Op::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from,
        } => Op::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from: Some(valid_from.unwrap_or(now_ms)),
        },
        Op::CloseEdge { id, valid_to } => Op::CloseEdge {
            id,
            valid_to: Some(valid_to.unwrap_or(now_ms)),
        },
        other => other,
    }
}

pub(crate) fn node_key(id: NodeId) -> [u8; 16] {
    id.as_u128().to_be_bytes()
}

/// Reads META `"oldest_seq"` (u64 LE) from an already-open META table; an
/// ABSENT key means the log was never compacted, so the floor is 1. Factored
/// out so `oldest_seq` (own read txn) and `read_ops` (shares the read txn with
/// its range scan for a consistent view) derive the floor identically.
fn read_oldest_seq(
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<u64, TopoError> {
    match meta.get("oldest_seq").map_err(storage_err)? {
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
            key[1..17].copy_from_slice(&id.as_u128().to_be_bytes());
        }
    }
    key
}

fn edge_key(id: EdgeId) -> [u8; 16] {
    id.as_u128().to_be_bytes()
}

fn read_embedding(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<Option<(String, Vec<f32>)>, TopoError> {
    let key = node_key(id);
    match table.get(key.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(v) => {
            let raw = crate::codec::unframe_value(v.value())?;
            postcard::from_bytes(&raw)
                .map(Some)
                .map_err(|e| TopoError::Encoding(e.to_string()))
        }
    }
}
fn read_node(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    embeddings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
    id: NodeId,
) -> Result<Option<NodeRecord>, TopoError> {
    let k = node_key(id);
    match table.get(k.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(v) => {
            let raw = crate::codec::unframe_value(v.value())?;
            let disk = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            let mut rec = crate::disk::node_from_disk(disk, dicts)?;
            rec.embedding = read_embedding(embeddings, id)?;
            Ok(Some(rec))
        }
    }
}
fn read_edge(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
    id: EdgeId,
) -> Result<Option<EdgeRecord>, TopoError> {
    let k = edge_key(id);
    match table.get(k.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(v) => {
            let raw = crate::codec::unframe_value(v.value())?;
            let disk = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(crate::disk::edge_from_disk(disk, dicts)?))
        }
    }
}
fn put_node(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    rec: &NodeRecord,
) -> Result<(), TopoError> {
    let raw = postcard::to_allocvec(&crate::disk::node_to_disk(rec, dict, dicts)?)
        .map_err(|e| TopoError::Encoding(e.to_string()))?;
    let f = crate::codec::frame_value(raw);
    table
        .insert(node_key(rec.id).as_slice(), f.as_slice())
        .map_err(storage_err)?;
    Ok(())
}
fn put_edge(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    rec: &EdgeRecord,
) -> Result<(), TopoError> {
    let raw = postcard::to_allocvec(&crate::disk::edge_to_disk(rec, dict, dicts)?)
        .map_err(|e| TopoError::Encoding(e.to_string()))?;
    let f = crate::codec::frame_value(raw);
    table
        .insert(edge_key(rec.id).as_slice(), f.as_slice())
        .map_err(storage_err)?;
    Ok(())
}
fn put_embedding(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    id: NodeId,
    model: &str,
    vector: &[f32],
) -> Result<(), TopoError> {
    let raw =
        postcard::to_allocvec(&(model, vector)).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let f = crate::codec::frame_value(raw);
    table
        .insert(node_key(id).as_slice(), f.as_slice())
        .map_err(storage_err)?;
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
    embeddings: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    op: &Op,
) -> Result<(), TopoError> {
    match op {
        Op::CreateNode {
            id,
            scope,
            label,
            props,
        } => {
            let rec = NodeRecord {
                id: *id,
                scope: *scope,
                label: label.clone(),
                props: props.clone(),
                embedding: None,
            };
            put_node(nodes, dict, dicts, &rec)
        }
        Op::SetNodeProps { id, props } => {
            let mut rec = read_node(nodes, embeddings, dicts, *id)?.ok_or_else(|| {
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
            put_node(nodes, dict, dicts, &rec)
        }
        Op::SetEmbedding { id, model, vector } => {
            read_node(nodes, embeddings, dicts, *id)?.ok_or_else(|| {
                TopoError::Rejected(format!("SetEmbedding: node {id:?} not found"))
            })?;
            put_embedding(embeddings, *id, model, vector)
        }
        Op::RemoveNode { id } => {
            let key = node_key(*id);
            let removed = nodes.remove(key.as_slice()).map_err(storage_err)?;
            if removed.is_none() {
                return Err(TopoError::Rejected(format!(
                    "RemoveNode: node {id:?} not found"
                )));
            }

            embeddings.remove(key.as_slice()).map_err(storage_err)?;
            // Remove incident edges, both directions. v0.1: linear scan is
            // acceptable; adjacency-assisted delete arrives with Task 5.
            let mut incident = Vec::new();
            for entry in edges.iter().map_err(storage_err)? {
                let (k, v) = entry.map_err(storage_err)?;
                let raw = crate::codec::unframe_value(v.value())?;
                let disk = postcard::from_bytes(raw.as_ref())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                let rec = crate::disk::edge_from_disk(disk, dicts)?;
                if rec.from == *id || rec.to == *id {
                    incident.push(k.value().to_vec());
                }
            }
            for key in incident {
                edges.remove(key.as_slice()).map_err(storage_err)?;
            }
            Ok(())
        }
        Op::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from,
        } => {
            let from_rec = read_node(nodes, embeddings, dicts, *from)?.ok_or_else(|| {
                TopoError::Rejected(format!("CreateEdge {id:?}: from node {from:?} not found"))
            })?;
            let to_rec = read_node(nodes, embeddings, dicts, *to)?.ok_or_else(|| {
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
            put_edge(edges, dict, dicts, &rec)
        }
        Op::CloseEdge { id, valid_to } => {
            let mut rec = read_edge(edges, dicts, *id)?
                .ok_or_else(|| TopoError::Rejected(format!("CloseEdge: edge {id:?} not found")))?;
            if rec.valid_to.is_some() {
                return Err(TopoError::Rejected(format!(
                    "CloseEdge: edge {id:?} already closed"
                )));
            }
            rec.valid_to = Some(
                valid_to
                    .expect("apply_op only runs on resolved ops (valid_to filled by resolve_op)"),
            );
            put_edge(edges, dict, dicts, &rec)
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
            Op::CreateNode {
                id: NodeId::new(),
                scope,
                label: "Memory".into(),
                props: Default::default(),
            },
            Op::CreateNode {
                id: NodeId::new(),
                scope,
                label: "Entity".into(),
                props: Default::default(),
            },
        ];
        let (first, last) = s.append_ops(&ops).unwrap();
        assert_eq!((first, last), (1, 2));
        let read = s.read_ops(1).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].1, ops[0]);
        assert_eq!(s.format_version().unwrap(), 2);
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
                meta.insert("format_version", 3u32.to_le_bytes().as_slice())
                    .unwrap();
            }
            tx.commit().unwrap();
        }

        // Reopening must now be rejected rather than silently accepted.
        // `.err()` drops the (non-`Debug`) `Storage` from the `Ok` arm.
        let err = Storage::open(&path).err().expect("reopen must be rejected");
        match err {
            TopoError::UnsupportedFormat {
                found: 3,
                supported: 2,
            } => {}
            other => {
                panic!("expected UnsupportedFormat {{ found: 3, supported: 2 }}, got {other:?}")
            }
        }
    }

    #[test]
    fn storage_report_counts_v2_tables_and_embeddings_are_cold() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let id = NodeId::from_u128(1);
        s.apply_batch(
            vec![
                Op::CreateNode {
                    id,
                    scope: Scope::Shared,
                    label: "Memory".into(),
                    props: Default::default(),
                },
                Op::SetEmbedding {
                    id,
                    model: "m".into(),
                    vector: vec![1.0; 64],
                },
            ],
            0,
        )
        .unwrap();
        let report = s.storage_report().unwrap();
        assert_eq!(
            report
                .iter()
                .find(|r| r.table == "embeddings")
                .unwrap()
                .rows,
            1
        );
        assert_eq!(
            s.load_node(id).unwrap().unwrap().embedding.unwrap().1.len(),
            64
        );
        s.apply_batch(vec![Op::RemoveNode { id }], 1).unwrap();
        assert_eq!(
            s.storage_report()
                .unwrap()
                .iter()
                .find(|r| r.table == "embeddings")
                .unwrap()
                .rows,
            0
        );
    }

    #[test]
    fn set_embedding_lands_in_record_and_rejects_missing_node() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let id = NodeId::new();
        s.apply_batch(
            vec![Op::CreateNode {
                id,
                scope,
                label: "M".into(),
                props: Default::default(),
            }],
            0,
        )
        .unwrap();
        s.apply_batch(
            vec![Op::SetEmbedding {
                id,
                model: "m".into(),
                vector: vec![1.0, 2.0, 3.0],
            }],
            0,
        )
        .unwrap();

        let rec = s.load_node(id).unwrap().unwrap();
        assert_eq!(rec.embedding, Some(("m".to_string(), vec![1.0, 2.0, 3.0])));

        // Embedding a node that doesn't exist rejects the whole batch.
        let err = s
            .apply_batch(
                vec![Op::SetEmbedding {
                    id: NodeId::new(),
                    model: "m".into(),
                    vector: vec![0.0],
                }],
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
