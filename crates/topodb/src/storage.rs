use crate::adj::{
    adj_close, adj_insert, adj_remove_all, adj_remove_edge, AdjEntryDisk, IN_ADJ, OUT_ADJ,
};
use crate::counters::AccessStats;
use crate::dict::{DictKind, Dicts, DICT};
use crate::error::{storage_err, TopoError};
use crate::fts::{doc_text, fts_update};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::index::IndexSpec;
use crate::op::Op;
use crate::prop_index::{index_node, unindex_node, PROP_INDEX};
use crate::scopes::{seed_shared, ScopeRegistry, SCOPES};
use crate::slots::{
    alloc_edge_slot, alloc_node_slot, remove_edge_mapping, remove_node_mapping, EDGE_IDS,
    EDGE_SLOTS, NODE_IDS, NODE_SLOTS,
};
use crate::state::{EdgeRecord, NodeRecord};
use redb::{Database, ReadableTable, Table, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

pub(crate) const OPS: TableDefinition<u64, &[u8]> = TableDefinition::new("ops");
pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub(crate) const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
pub(crate) const EDGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edges");
/// Inverted index: `scope_id.to_be_bytes() ++ term` UTF-8 bytes → framed
/// delta-varint `(slot_delta, tf)` pairs (ascending by node slot), maintained
/// transactionally by `fts_update`. Opened in `open_with`. Re-keyed from the
/// v2 `scope_key(scope) ++ term` / postcard `Vec<(NodeId, u32)>` layout by
/// W2b (v3 spec §3, FTS rows) — see `fts.rs`'s module doc comment.
pub(crate) const POSTINGS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("postings");
/// Per-document token length: 8-byte BE node slot → postcard `u32`. Opened in
/// `open_with`. Re-keyed from the v2 ULID node key by W2b.
pub(crate) const FTS_DOCS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fts_docs");
/// Per-scope corpus statistics: `scope_id.to_be_bytes()` (4-byte u32 BE) →
/// postcard `(u64, u64)` = `(doc_count, total_len)`. Opened in `open_with`,
/// maintained transactionally by `fts_update`. Re-keyed from the v2
/// `scope_key(scope)` layout by W2b; corpus stats are sourced per scope so
/// that documents in one scope never shift another scope's BM25 df/avgdl.
pub(crate) const FTS_STATS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("fts_stats");
/// Auxiliary per-node access statistics, keyed by the same 8-byte dense slot
/// key as NODES. Deliberately *outside* the op log: never appended to OPS
/// and never broadcast to the change feed. `rebuild_state_from_ops` DOES
/// touch this table (it must — replay can reassign slots), but only to
/// re-key existing rows by node identity; it never resets counts to zero
/// (see that function's doc comment).
pub(crate) const COUNTERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("counters");
/// Cold vector rows: node key -> framed postcard (model, vector).
pub(crate) const EMBEDDINGS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("embeddings");

pub const FORMAT_VERSION: u32 = 3;

/// One EMBEDDINGS row joined with its node's id/scope: `(model, scope, id,
/// vector)`. See `Storage::all_embeddings_with_scope`.
pub(crate) type EmbeddingRow = (String, Scope, NodeId, Vec<f32>);

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
    /// rebuild, and by `Db::index_spec` — the single source of truth for the
    /// declared spec (there is no separate in-memory copy).
    pub(crate) spec: Arc<IndexSpec>,
    pub(crate) dicts: RwLock<Dicts>,
    pub(crate) scope_registry: RwLock<ScopeRegistry>,
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
        Self::open_with_options(path, spec, crate::db::DbOptions::default())
    }

    /// Like `open_with`, but threads `options` to the underlying redb
    /// `Builder` before the database file is created/opened — currently just
    /// `cache_size_bytes` -> `Builder::set_cache_size`. `None` leaves redb's
    /// own default untouched, so `DbOptions::default()` (used by `open_with`)
    /// behaves identically to the old bare `Database::create(path)` call.
    pub(crate) fn open_with_options(
        path: impl AsRef<Path>,
        spec: Arc<IndexSpec>,
        options: crate::db::DbOptions,
    ) -> Result<Self, TopoError> {
        let mut builder = Database::builder();
        if let Some(bytes) = options.cache_size_bytes {
            builder.set_cache_size(bytes);
        }
        let db = builder.create(path).map_err(storage_err)?;
        let s = Self {
            db,
            spec: spec.clone(),
            dicts: RwLock::new(Dicts::default()),
            scope_registry: RwLock::new(ScopeRegistry::default()),
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
            tx.open_table(NODE_SLOTS).map_err(storage_err)?;
            tx.open_table(NODE_IDS).map_err(storage_err)?;
            tx.open_table(EDGE_SLOTS).map_err(storage_err)?;
            tx.open_table(EDGE_IDS).map_err(storage_err)?;
            let mut scopes = tx.open_table(SCOPES).map_err(storage_err)?;
            seed_shared(&mut scopes)?;
            tx.open_table(OUT_ADJ).map_err(storage_err)?;
            tx.open_table(IN_ADJ).map_err(storage_err)?;
            tx.open_table(PROP_INDEX).map_err(storage_err)?;
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
            Some(3) => {}
            Some(2) => {
                let mut meta = tx.open_table(META).map_err(storage_err)?;
                let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
                let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
                let mut embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
                let mut counters = tx.open_table(COUNTERS).map_err(storage_err)?;
                let mut scopes = tx.open_table(SCOPES).map_err(storage_err)?;
                let mut node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
                let mut node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
                let mut edge_slots = tx.open_table(EDGE_SLOTS).map_err(storage_err)?;
                let mut edge_ids = tx.open_table(EDGE_IDS).map_err(storage_err)?;
                let mut out_adj = tx.open_table(OUT_ADJ).map_err(storage_err)?;
                let mut in_adj = tx.open_table(IN_ADJ).map_err(storage_err)?;
                let mut prop_index = tx.open_table(PROP_INDEX).map_err(storage_err)?;
                let mut dict = tx.open_table(DICT).map_err(storage_err)?;
                let mut dicts = Dicts::load_from_table(&dict)?;
                // FTS tables re-keyed by this migration too (v3 spec §3, FTS
                // rows): a v2 file's postings/fts_docs/fts_stats are still
                // ULID/`scope_key`-keyed (pre-W2b layout), byte-incompatible
                // with the interned-scope-id/dense-slot layout `fts.rs` reads
                // post-migration. See `migrate_v2_to_v3`'s doc comment.
                let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
                let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
                let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
                crate::migrate_v3::migrate_v2_to_v3(
                    spec.clone(),
                    &mut meta,
                    &mut nodes,
                    &mut edges,
                    &mut embeddings,
                    &mut counters,
                    &mut dict,
                    &mut dicts,
                    &mut scopes,
                    &mut node_slots,
                    &mut node_ids,
                    &mut edge_slots,
                    &mut edge_ids,
                    &mut out_adj,
                    &mut in_adj,
                    &mut prop_index,
                    &mut postings,
                    &mut docs,
                    &mut stats,
                )?;
                meta.insert("format_version", FORMAT_VERSION.to_le_bytes().as_slice())
                    .map_err(storage_err)?;
            }
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
                meta.insert("format_version", 2u32.to_le_bytes().as_slice())
                    .map_err(storage_err)?;
                drop(meta);
                let mut meta = tx.open_table(META).map_err(storage_err)?;
                let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
                let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
                let mut embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
                let mut counters = tx.open_table(COUNTERS).map_err(storage_err)?;
                let mut scopes = tx.open_table(SCOPES).map_err(storage_err)?;
                let mut node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
                let mut node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
                let mut edge_slots = tx.open_table(EDGE_SLOTS).map_err(storage_err)?;
                let mut edge_ids = tx.open_table(EDGE_IDS).map_err(storage_err)?;
                let mut out_adj = tx.open_table(OUT_ADJ).map_err(storage_err)?;
                let mut in_adj = tx.open_table(IN_ADJ).map_err(storage_err)?;
                let mut prop_index = tx.open_table(PROP_INDEX).map_err(storage_err)?;
                let mut dict = tx.open_table(DICT).map_err(storage_err)?;
                let mut dicts = Dicts::load_from_table(&dict)?;
                // FTS tables re-keyed by this migration too (v3 spec §3, FTS
                // rows): a v2 file's postings/fts_docs/fts_stats are still
                // ULID/`scope_key`-keyed (pre-W2b layout), byte-incompatible
                // with the interned-scope-id/dense-slot layout `fts.rs` reads
                // post-migration. See `migrate_v2_to_v3`'s doc comment.
                let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
                let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
                let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
                crate::migrate_v3::migrate_v2_to_v3(
                    spec.clone(),
                    &mut meta,
                    &mut nodes,
                    &mut edges,
                    &mut embeddings,
                    &mut counters,
                    &mut dict,
                    &mut dicts,
                    &mut scopes,
                    &mut node_slots,
                    &mut node_ids,
                    &mut edge_slots,
                    &mut edge_ids,
                    &mut out_adj,
                    &mut in_adj,
                    &mut prop_index,
                    &mut postings,
                    &mut docs,
                    &mut stats,
                )?;
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
        *s.scope_registry
            .write()
            .expect("scope registry lock poisoned") = ScopeRegistry::load(&r)?;
        drop(r);
        s.ensure_index_spec()?;
        Ok(s)
    }

    /// Reconciles the on-disk text AND equality indexes with the `IndexSpec`
    /// this storage was opened with, and persists the full spec under META
    /// `"index_spec"`.
    ///
    /// The stored spec has BOTH its `equality` and `text` lists sorted by
    /// `(label, prop)` before encoding, so a mere reordering of declarations
    /// never looks like a change. A change in EITHER list triggers a reindex:
    /// unlike v2 (where `graph.rs` rebuilt the equality index in RAM on every
    /// open — that module is gone), v3's PROP_INDEX is an on-disk table that
    /// is only ever maintained incrementally by the write path
    /// (`apply_batch`/`rebuild_state_from_ops`), so a newly declared
    /// `(label, prop)` pair has no rows for pre-existing nodes until this
    /// reconciliation rebuilds it, and a removed-then-reintroduced
    /// declaration must have its stale rows (written while the declaration
    /// was absent and props kept changing) purged rather than resurrected.
    ///
    /// Reindex decision (one write transaction):
    /// - Legacy Plan-2 layout (`"fts_spec"` present): the on-disk postings are
    ///   keyed by bare term (no scope prefix) and corpus stats live in the
    ///   `"fts_doc_count"`/`"fts_total_len"` META counters — incompatible with
    ///   the per-scope layout. Always drain + full reindex, and delete the three
    ///   legacy keys.
    /// - New layout (`"index_spec"` present): reindex iff the stored text list
    ///   OR the stored equality list differs from the incoming one.
    /// - Plan-1 file (neither key): reindex iff the incoming text list is
    ///   non-empty (nothing was ever indexed).
    ///
    /// A reindex drains POSTINGS/FTS_DOCS/FTS_STATS/PROP_INDEX and rebuilds by
    /// scanning NODES: FTS rows via `fts_update` and PROP_INDEX rows via
    /// `prop_index::index_node` (threading each node's slot and its already-
    /// interned scope id, read straight off the row — see the loop below), so
    /// the new postings are scope-id-prefixed, FTS_STATS is per-scope-id, and
    /// PROP_INDEX reflects exactly the current spec against current node
    /// state (no stale rows survive a remove-then-reintroduce cycle, since the
    /// whole table is drained first).
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
                        (
                            stored.text != incoming.text || stored.equality != incoming.equality,
                            false,
                        )
                    }
                    None => (!incoming.text.is_empty(), false),
                }
            }
        };

        if needs_reindex {
            let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
            let mut prop_index = tx.open_table(PROP_INDEX).map_err(storage_err)?;
            postings.retain(|_, _| false).map_err(storage_err)?;
            docs.retain(|_, _| false).map_err(storage_err)?;
            stats.retain(|_, _| false).map_err(storage_err)?;
            prop_index.retain(|_, _| false).map_err(storage_err)?;

            let nodes = tx.open_table(NODES).map_err(storage_err)?;
            let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
            let dicts = self.dicts.read().expect("dict lock poisoned");
            let scopes = self
                .scope_registry
                .read()
                .expect("scope registry lock poisoned");
            for entry in nodes.iter().map_err(storage_err)? {
                let (key_guard, value_guard) = entry.map_err(storage_err)?;
                let key: [u8; 8] = key_guard
                    .value()
                    .try_into()
                    .map_err(|_| TopoError::Encoding("bad node slot key".into()))?;
                let slot = u64::from_be_bytes(key);
                let raw = crate::codec::unframe_value(value_guard.value())?;
                let disk: crate::disk::NodeRecordDiskV3 = postcard::from_bytes(raw.as_ref())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                // The row's scope id is already the interned v3 id — no need
                // to re-resolve/re-intern through `ScopeRegistry`, so this
                // loop stays read-only on `scopes` (matches the `Some(2)`/
                // `Some(1)` migration call sites, which never need a `&mut
                // ScopeRegistry` here either).
                let scope_id = disk.scope;
                let mut rec = crate::disk::node_from_disk_v3(disk, &dicts, &scopes)?;
                rec.embedding = read_embedding(&embeddings, slot)?;
                let new_text = doc_text(&self.spec, &rec);
                fts_update(
                    &mut postings,
                    &mut docs,
                    &mut stats,
                    scope_id,
                    slot,
                    None,
                    new_text.as_deref(),
                )?;
                index_node(&mut prop_index, &self.spec, &dicts, &rec, slot)?;
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
            // v3 tables (chunked adjacency, dense slot maps, prop equality
            // index, scope registry) — added for the v3 size gate, which
            // needs edges+out_adj+in_adj split out (see BENCHMARKS.md v3).
            bytes(&tx, OUT_ADJ, "out_adj")?,
            bytes(&tx, IN_ADJ, "in_adj")?,
            bytes(&tx, PROP_INDEX, "prop_index")?,
            bytes(&tx, NODE_SLOTS, "node_slots")?,
            bytes(&tx, NODE_IDS, "node_ids")?,
            bytes(&tx, EDGE_SLOTS, "edge_slots")?,
            bytes(&tx, EDGE_IDS, "edge_ids")?,
            bytes(&tx, SCOPES, "scopes")?,
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
        let mut scope_registry = self
            .scope_registry
            .write()
            .expect("scope registry lock poisoned");
        let scope_read = self.db.begin_read().map_err(storage_err)?;
        *scope_registry = ScopeRegistry::load(&scope_read)?;
        drop(scope_read);
        let tx = self.db.begin_write().map_err(storage_err)?;
        // Text-index edits collected during the op loop and applied AFTER every
        // op has succeeded — still inside this transaction, so the postings
        // ride the batch's atomicity (a later failing op aborts the whole txn,
        // leaving the index untouched). `old_text` is captured BEFORE `apply_op`
        // mutates the record.
        // Each edit also carries the node's interned scope id and dense slot
        // (immutable — old and new scope/slot are always identical), needed to
        // key per-scope-id, per-slot postings/stats/docs (v3 FTS layout).
        let mut fts_edits: Vec<(u32, u64, Option<String>, Option<String>)> = Vec::new();
        {
            let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
            let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
            let mut embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
            let mut dict_table = tx.open_table(DICT).map_err(storage_err)?;
            let mut slot_meta = tx.open_table(META).map_err(storage_err)?;
            let mut node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
            let mut node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
            let mut edge_slots = tx.open_table(EDGE_SLOTS).map_err(storage_err)?;
            let mut edge_ids = tx.open_table(EDGE_IDS).map_err(storage_err)?;
            let mut scopes_table = tx.open_table(SCOPES).map_err(storage_err)?;
            let mut out_adj = tx.open_table(OUT_ADJ).map_err(storage_err)?;
            let mut in_adj = tx.open_table(IN_ADJ).map_err(storage_err)?;
            let mut prop_index = tx.open_table(PROP_INDEX).map_err(storage_err)?;
            for op in &resolved {
                // `pre` carries (id, scope, pre_slot, old_text). For CreateNode
                // the scope comes from the op and the slot isn't allocated yet
                // (resolved after `apply_op` below); for existing-node ops
                // scope/slot come from the record/mapping read before mutation
                // — captured HERE for RemoveNode specifically, because
                // `apply_op` erases the NODE_SLOTS mapping as part of removal,
                // so the slot is unrecoverable afterward.
                let pre: Option<(NodeId, Scope, Option<u64>, Option<String>)> = match op {
                    Op::CreateNode { id, scope, .. } => Some((*id, *scope, None, None)),
                    Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => {
                        match read_node(
                            &nodes,
                            &embeddings,
                            &dicts,
                            &scope_registry,
                            &node_slots,
                            *id,
                        )? {
                            Some(rec) => {
                                let slot = crate::slots::node_slot(&node_slots, *id)?;
                                Some((*id, rec.scope, slot, doc_text(&self.spec, &rec)))
                            }
                            None => None,
                        }
                    }
                    // SetEmbedding never changes text; edge ops carry none.
                    _ => None,
                };
                let old_index_node = match op {
                    Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => read_node(
                        &nodes,
                        &embeddings,
                        &dicts,
                        &scope_registry,
                        &node_slots,
                        *id,
                    )?,
                    _ => None,
                };
                if let Some(node) = &old_index_node {
                    if let Some(slot) = crate::slots::node_slot(&node_slots, node.id)? {
                        unindex_node(&mut prop_index, &self.spec, &dicts, node, slot)?;
                    }
                }
                apply_op(
                    &mut nodes,
                    &mut edges,
                    &mut embeddings,
                    &mut dict_table,
                    &mut dicts,
                    &mut slot_meta,
                    &mut node_slots,
                    &mut node_ids,
                    &mut edge_slots,
                    &mut edge_ids,
                    &mut out_adj,
                    &mut in_adj,
                    &mut scopes_table,
                    &mut scope_registry,
                    op,
                )?;
                if !matches!(op, Op::RemoveNode { .. }) {
                    let id = match op {
                        Op::CreateNode { id, .. } | Op::SetNodeProps { id, .. } => Some(*id),
                        _ => None,
                    };
                    if let Some(id) = id {
                        if let Some(node) = read_node(
                            &nodes,
                            &embeddings,
                            &dicts,
                            &scope_registry,
                            &node_slots,
                            id,
                        )? {
                            if let Some(slot) = crate::slots::node_slot(&node_slots, id)? {
                                index_node(&mut prop_index, &self.spec, &dicts, &node, slot)?;
                            }
                        }
                    }
                }
                if let Some((id, scope, pre_slot, old_text)) = pre {
                    let new_text = match op {
                        Op::RemoveNode { .. } => None,
                        _ => read_node(
                            &nodes,
                            &embeddings,
                            &dicts,
                            &scope_registry,
                            &node_slots,
                            id,
                        )?
                        .and_then(|rec| doc_text(&self.spec, &rec)),
                    };
                    // CreateNode's slot is allocated inside `apply_op` above,
                    // so it only resolves now; every other op captured its
                    // slot pre-mutation (see the `pre` comment above).
                    let slot = match pre_slot {
                        Some(s) => s,
                        None => crate::slots::node_slot(&node_slots, id)?.ok_or_else(|| {
                            TopoError::Encoding(
                                "fts edit: node slot missing after CreateNode".into(),
                            )
                        })?,
                    };
                    // Idempotent: CreateNode/SetNodeProps already interned
                    // this exact scope via `put_node` a few lines up (inside
                    // `apply_op`), so this is a HashMap lookup, not a fresh
                    // allocation — except for RemoveNode, where it's the
                    // scope's only remaining reference in this op, but the
                    // scope was necessarily interned when the node was
                    // created, so it still resolves to the same id.
                    let scope_id = scope_registry.intern(&mut scopes_table, scope)?;
                    fts_edits.push((scope_id, slot, old_text, new_text));
                }
            }
        }
        {
            let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
            let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
            let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;
            for (scope_id, slot, old_text, new_text) in &fts_edits {
                fts_update(
                    &mut postings,
                    &mut docs,
                    &mut stats,
                    *scope_id,
                    *slot,
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

    /// One-transaction indexed lookup: PROP_INDEX prefix scan + record fetches
    /// share a single `begin_read`, so the result is one consistent view — a
    /// node whose indexed prop changed between two separate txns can never be
    /// returned as a stale match.
    pub(crate) fn load_nodes_by_index(
        &self,
        prop_key: u32,
        value: &crate::index::IndexValue,
    ) -> Result<Vec<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let index = tx.open_table(PROP_INDEX).map_err(storage_err)?;
        let slots = crate::prop_index::lookup(&index, prop_key, value)?;
        drop(index);
        let nodes = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        let mut out = Vec::new();
        for slot in slots {
            if let Some(rec) = read_node_by_slot(&nodes, &embeddings, &dicts, &scopes, slot)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Crate-internal only (`Storage` isn't re-exported); this `pub` is inert
    /// outside the crate. Used by the scoped point-lookup read path
    /// (`Db::node`, `Db::access_stats`) and exercised directly by unit tests.
    pub fn load_node(&self, id: NodeId) -> Result<Option<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        read_node(&table, &embeddings, &dicts, &scopes, &node_slots, id)
    }

    /// See `load_node`.
    #[allow(dead_code)]
    pub fn load_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(EDGES).map_err(storage_err)?;
        let edge_slots = tx.open_table(EDGE_SLOTS).map_err(storage_err)?;
        let node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        read_edge(&table, &dicts, &scopes, &edge_slots, &node_ids, id)
    }

    /// Crate-internal full scan — used to rebuild in-memory adjacency. Not
    /// public API: callers should go through the (future) query layer.
    pub(crate) fn all_nodes(&self) -> Result<Vec<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        let mut out = Vec::new();
        for entry in table.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let key: [u8; 8] = k
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad node slot key".into()))?;
            let slot = u64::from_be_bytes(key);
            let raw = crate::codec::unframe_value(v.value())?;
            let disk = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            let mut rec = crate::disk::node_from_disk_v3(disk, &dicts, &scopes)?;
            rec.embedding = read_embedding(&embeddings, slot)?;
            out.push(rec);
        }
        Ok(out)
    }

    /// Scans EMBEDDINGS and joins each row with its node's id/scope (read off
    /// the NODES row at the same slot). This is the ONLY remaining open-time
    /// scan the engine performs (SP2 removes it too) — `VectorIndex::from_storage`
    /// uses it to seed the in-RAM slab index at `Db::open` and on explicit
    /// rebuild. Bounded by the EMBEDDINGS table's size, not NODES' — cheaper
    /// than a full node scan whenever embeddings are a minority of nodes. An
    /// EMBEDDINGS row whose NODES row is missing (should not happen — the two
    /// are maintained together) is silently skipped rather than surfaced as
    /// corruption, matching this being a best-effort index-seeding scan.
    // SP2: last open-time scan
    pub(crate) fn all_embeddings_with_scope(&self) -> Result<Vec<EmbeddingRow>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let nodes = tx.open_table(NODES).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        let mut out = Vec::new();
        for entry in embeddings.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let key: [u8; 8] = k
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad embedding slot key".into()))?;
            let slot = u64::from_be_bytes(key);
            let raw = crate::codec::unframe_value(v.value())?;
            let (model, vector): (String, Vec<f32>) =
                postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
            let Some(node_v) = nodes.get(slot_key(slot).as_slice()).map_err(storage_err)? else {
                continue;
            };
            let raw_node = crate::codec::unframe_value(node_v.value())?;
            let disk: crate::disk::NodeRecordDiskV3 = postcard::from_bytes(raw_node.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            let rec = crate::disk::node_from_disk_v3(disk, &dicts, &scopes)?;
            out.push((model, rec.scope, rec.id, vector));
        }
        Ok(out)
    }

    /// Bulk point lookup: every id in `ids` that currently resolves to a live
    /// node, in one read transaction (a missing id is simply absent from the
    /// result, not an error). Used by the applier to capture pre-batch node
    /// state (scope, embedding) for dim/edge-scope pre-validation and vector
    /// slab maintenance — reading it BEFORE `apply_batch` runs, since storage
    /// holds only the post-batch state once `apply_batch` has committed — and
    /// by `search_vector` to resolve slab hits back to `NodeRecord`s.
    pub(crate) fn load_nodes(
        &self,
        ids: &std::collections::HashSet<NodeId>,
    ) -> Result<std::collections::HashMap<NodeId, NodeRecord>, TopoError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
        let node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        for &id in ids {
            if let Some(rec) = read_node(&table, &embeddings, &dicts, &scopes, &node_slots, id)? {
                out.insert(id, rec);
            }
        }
        Ok(out)
    }

    pub(crate) fn all_edges(&self) -> Result<Vec<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(EDGES).map_err(storage_err)?;
        let node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
        let dicts = self.dicts.read().expect("dict lock poisoned");
        let scopes = self
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        let mut out = Vec::new();
        for entry in table.iter().map_err(storage_err)? {
            let (_, v) = entry.map_err(storage_err)?;
            let raw = crate::codec::unframe_value(v.value())?;
            let disk = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push(crate::disk::edge_from_disk_v3(
                disk, &dicts, &scopes, &node_ids,
            )?);
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
            let node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
            for (id, n, ts) in bumps {
                // The bump channel still carries ULIDs; a ULID that no longer
                // resolves (node removed since the bump was queued) is
                // silently dropped — no orphan COUNTERS row under a slot that
                // was never (re-)assigned to this ULID.
                let Some(slot) = crate::slots::node_slot(&node_slots, *id)? else {
                    continue;
                };
                let key = slot_key(slot);
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
    /// counted (or no longer exists). Scope gating is the caller's
    /// responsibility (`Db::access_stats` checks node existence/scope first);
    /// this is a pure COUNTERS lookup.
    pub(crate) fn read_counter(&self, id: NodeId) -> Result<Option<AccessStats>, TopoError> {
        let tx = self.db.begin_read().map_err(storage_err)?;
        let table = tx.open_table(COUNTERS).map_err(storage_err)?;
        let node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
        let Some(slot) = crate::slots::node_slot(&node_slots, id)? else {
            return Ok(None);
        };
        let key = slot_key(slot);
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
    /// COUNTERS is preserved across the rebuild, but NOT by leaving its rows
    /// slot-keyed and untouched: replay reassigns node slots in OP-LOG
    /// order, which need not match the slot order the table was in before
    /// the rebuild (a migrated v2 file assigned slots in v2-ULID iteration
    /// order; a create/remove/create sequence burns and reassigns slots out
    /// of ULID order too). Reusing old slot numbers verbatim would silently
    /// transfer one node's access stats onto a DIFFERENT, unrelated node
    /// that happens to land on the same slot after replay. Instead, COUNTERS
    /// is snapshotted by node IDENTITY (ULID, resolved through the OLD
    /// NODE_IDS mapping) before anything is drained, then every surviving
    /// counter is re-inserted under that same node's NEW slot once replay
    /// completes; a counter whose ULID no longer exists after replay has no
    /// node to attribute it to and is dropped.
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
        let mut scope_registry = self
            .scope_registry
            .write()
            .expect("scope registry lock poisoned");
        // The whole rebuild runs inside this closure so any `?` bail-out below
        // is caught here rather than escaping the function directly: a write
        // transaction that errors mid-body aborts cleanly on disk, but
        // `dicts`/`scope_registry` may already have been cleared/replaced in
        // memory ahead of the failure (`dicts.clear()` and the scope-registry
        // reload both happen before the ops replay that can itself fail). On
        // any error, both in-memory mirrors are reloaded from the last
        // COMMITTED rows so they never drift from what's actually on disk —
        // the same recovery `apply_batch` performs on ENTRY (via a fresh
        // `Dicts::load`/`ScopeRegistry::load` read, storage.rs:705-718)
        // applied here on the error EXIT instead.
        let result: Result<(), TopoError> = (|| {
            let tx = self.db.begin_write().map_err(storage_err)?;
            {
                let mut nodes = tx.open_table(NODES).map_err(storage_err)?;
                let mut edges = tx.open_table(EDGES).map_err(storage_err)?;
                let mut embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;
                let mut dict_table = tx.open_table(DICT).map_err(storage_err)?;
                let mut slot_meta = tx.open_table(META).map_err(storage_err)?;
                let mut node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
                let mut node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
                let mut edge_slots = tx.open_table(EDGE_SLOTS).map_err(storage_err)?;
                let mut edge_ids = tx.open_table(EDGE_IDS).map_err(storage_err)?;
                let mut scopes_table = tx.open_table(SCOPES).map_err(storage_err)?;
                let mut out_adj = tx.open_table(OUT_ADJ).map_err(storage_err)?;
                let mut in_adj = tx.open_table(IN_ADJ).map_err(storage_err)?;
                let mut prop_index = tx.open_table(PROP_INDEX).map_err(storage_err)?;
                let mut counters = tx.open_table(COUNTERS).map_err(storage_err)?;
                // The text index is derived from state, so it is drained and rebuilt
                // alongside NODES/EDGES through the very same `fts_update` used on the
                // write path — no parallel maintenance logic.
                let mut postings = tx.open_table(POSTINGS).map_err(storage_err)?;
                let mut docs = tx.open_table(FTS_DOCS).map_err(storage_err)?;
                let mut stats = tx.open_table(FTS_STATS).map_err(storage_err)?;

                // Snapshot COUNTERS by node IDENTITY (ULID) before anything is
                // drained — `node_ids` is still the OLD (pre-rebuild)
                // slot->ULID mapping at this point. See the function doc
                // comment for why slot-keyed rows can't just be left in place.
                let mut old_counters: HashMap<NodeId, Vec<u8>> = HashMap::new();
                for entry in counters.iter().map_err(storage_err)? {
                    let (k, v) = entry.map_err(storage_err)?;
                    let slot_bytes: [u8; 8] = k
                        .value()
                        .try_into()
                        .map_err(|_| TopoError::Encoding("bad counters slot key".into()))?;
                    let slot = u64::from_be_bytes(slot_bytes);
                    if let Some(ulid) = crate::slots::node_ulid(&node_ids, slot)? {
                        old_counters.insert(ulid, v.value().to_vec());
                    }
                }

                nodes.retain(|_, _| false).map_err(storage_err)?;
                edges.retain(|_, _| false).map_err(storage_err)?;
                embeddings.retain(|_, _| false).map_err(storage_err)?;
                dict_table.retain(|_, _| false).map_err(storage_err)?;
                slot_meta.remove("next_node_slot").map_err(storage_err)?;
                slot_meta.remove("next_edge_slot").map_err(storage_err)?;
                node_slots.retain(|_, _| false).map_err(storage_err)?;
                node_ids.retain(|_, _| false).map_err(storage_err)?;
                edge_slots.retain(|_, _| false).map_err(storage_err)?;
                edge_ids.retain(|_, _| false).map_err(storage_err)?;
                dicts.clear();
                scopes_table.retain(|_, _| false).map_err(storage_err)?;
                out_adj.retain(|_, _| false).map_err(storage_err)?;
                in_adj.retain(|_, _| false).map_err(storage_err)?;
                prop_index.retain(|_, _| false).map_err(storage_err)?;
                counters.retain(|_, _| false).map_err(storage_err)?;
                seed_shared(&mut scopes_table)?;
                *scope_registry = ScopeRegistry::load_table_for_rebuild(&scopes_table)?;
                postings.retain(|_, _| false).map_err(storage_err)?;
                docs.retain(|_, _| false).map_err(storage_err)?;
                stats.retain(|_, _| false).map_err(storage_err)?;

                let ops_table = tx.open_table(OPS).map_err(storage_err)?;
                for entry in ops_table.iter().map_err(storage_err)? {
                    let (_, v) = entry.map_err(storage_err)?;
                    let op: Op = postcard::from_bytes(v.value())
                        .map_err(|e| TopoError::Encoding(e.to_string()))?;
                    // Same (id, scope, pre_slot, old_text) derivation as
                    // `apply_batch`: old_text read BEFORE `apply_op` mutates the
                    // record; scope from the op (create) or the pre-mutation
                    // record; slot captured pre-mutation too (RemoveNode erases
                    // the NODE_SLOTS mapping inside `apply_op`, so it's
                    // unrecoverable afterward), left `None` for CreateNode since
                    // the slot isn't allocated until `apply_op` runs.
                    let pre: Option<(NodeId, Scope, Option<u64>, Option<String>)> =
                        match &op {
                            Op::CreateNode { id, scope, .. } => Some((*id, *scope, None, None)),
                            Op::SetNodeProps { id, .. } | Op::RemoveNode { id } => match read_node(
                                &nodes,
                                &embeddings,
                                &dicts,
                                &scope_registry,
                                &node_slots,
                                *id,
                            )? {
                                Some(rec) => {
                                    let slot = crate::slots::node_slot(&node_slots, *id)?;
                                    Some((*id, rec.scope, slot, doc_text(&self.spec, &rec)))
                                }
                                None => None,
                            },
                            _ => None,
                        };
                    apply_op(
                        &mut nodes,
                        &mut edges,
                        &mut embeddings,
                        &mut dict_table,
                        &mut dicts,
                        &mut slot_meta,
                        &mut node_slots,
                        &mut node_ids,
                        &mut edge_slots,
                        &mut edge_ids,
                        &mut out_adj,
                        &mut in_adj,
                        &mut scopes_table,
                        &mut scope_registry,
                        &op,
                    )?;
                    if !matches!(op, Op::RemoveNode { .. }) {
                        let id = match &op {
                            Op::CreateNode { id, .. } | Op::SetNodeProps { id, .. } => Some(*id),
                            _ => None,
                        };
                        if let Some(id) = id {
                            if let Some(node) = read_node(
                                &nodes,
                                &embeddings,
                                &dicts,
                                &scope_registry,
                                &node_slots,
                                id,
                            )? {
                                if let Some(slot) = crate::slots::node_slot(&node_slots, id)? {
                                    index_node(&mut prop_index, &self.spec, &dicts, &node, slot)?;
                                }
                            }
                        }
                    }
                    if let Some((id, scope, pre_slot, old_text)) = pre {
                        let new_text = match &op {
                            Op::RemoveNode { .. } => None,
                            _ => read_node(
                                &nodes,
                                &embeddings,
                                &dicts,
                                &scope_registry,
                                &node_slots,
                                id,
                            )?
                            .and_then(|rec| doc_text(&self.spec, &rec)),
                        };
                        let slot = match pre_slot {
                            Some(s) => s,
                            None => crate::slots::node_slot(&node_slots, id)?.ok_or_else(|| {
                                TopoError::Encoding(
                                    "fts replay: node slot missing after CreateNode".into(),
                                )
                            })?,
                        };
                        // Idempotent re-intern — see `apply_batch`'s identical
                        // comment; the scope was already interned when the node
                        // was created (or an earlier op on the same node).
                        let scope_id = scope_registry.intern(&mut scopes_table, scope)?;
                        fts_update(
                            &mut postings,
                            &mut docs,
                            &mut stats,
                            scope_id,
                            slot,
                            old_text.as_deref(),
                            new_text.as_deref(),
                        )?;
                    }
                }

                // Replay complete — re-key every preserved counter under its
                // node's NEW slot (`node_slots` is now the freshly-rebuilt
                // mapping). A ULID with no NEW slot (removed by replay and never
                // recreated) has no node left to attribute the counter to, so it
                // is dropped rather than carried forward as an orphan row.
                for (ulid, bytes) in &old_counters {
                    if let Some(new_slot) = crate::slots::node_slot(&node_slots, *ulid)? {
                        counters
                            .insert(slot_key(new_slot).as_slice(), bytes.as_slice())
                            .map_err(storage_err)?;
                    }
                }
            }
            tx.commit().map_err(storage_err)?;
            Ok(())
        })();
        if result.is_err() {
            if let Ok(r) = self.db.begin_read() {
                if let Ok(d) = Dicts::load(&r) {
                    *dicts = d;
                }
                if let Ok(sr) = ScopeRegistry::load(&r) {
                    *scope_registry = sr;
                }
            }
        }
        result
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

/// 8-byte BE dense-slot key used by the v3 record/sidecar tables (NODES,
/// EDGES, EMBEDDINGS, COUNTERS, and — as of W2b — `fts.rs`'s FTS_DOCS).
/// `node_key` (ULID key, above) remains in use only by `migrate.rs` (v1->v2,
/// frozen) and `migrate_v3.rs` (reading OLD v2-keyed EMBEDDINGS/COUNTERS rows
/// before they're re-keyed).
pub(crate) fn slot_key(slot: u64) -> [u8; 8] {
    slot.to_be_bytes()
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

fn read_embedding(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<(String, Vec<f32>)>, TopoError> {
    let key = slot_key(slot);
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
/// Direct slot-keyed NODES fetch — used once the caller already has the slot
/// (e.g. from a PROP_INDEX lookup, or `search_text`'s postings), skipping the
/// ULID->slot resolution that `read_node` performs. `pub(crate)` so `fts.rs`
/// can resolve `search_text` hits straight from the read transaction it
/// already has open, with no separate snapshot hop.
pub(crate) fn read_node_by_slot(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    embeddings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
    scopes: &ScopeRegistry,
    slot: u64,
) -> Result<Option<NodeRecord>, TopoError> {
    let k = slot_key(slot);
    match table.get(k.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(v) => {
            let raw = crate::codec::unframe_value(v.value())?;
            let disk = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            let mut rec = crate::disk::node_from_disk_v3(disk, dicts, scopes)?;
            rec.embedding = read_embedding(embeddings, slot)?;
            Ok(Some(rec))
        }
    }
}
/// ULID-keyed NODES fetch with a two-cause miss split:
/// - no NODE_SLOTS mapping at all → `Ok(None)`, ordinary not-found (callers
///   surface it as `Rejected`, exactly like a lookup of an id that never
///   existed);
/// - a mapping that resolves to a slot whose NODES row is absent →
///   `Err(TopoError::Encoding)`. The mapping and the record row are written
///   and removed atomically in every write path, so they can only diverge if
///   the file is damaged — that is data-integrity corruption and must
///   surface loudly, never masquerade as a routine "not found".
fn read_node(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    embeddings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
    scopes: &ScopeRegistry,
    node_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<Option<NodeRecord>, TopoError> {
    let Some(slot) = crate::slots::node_slot(node_slots, id)? else {
        return Ok(None);
    };
    match read_node_by_slot(table, embeddings, dicts, scopes, slot)? {
        Some(rec) => Ok(Some(rec)),
        None => Err(TopoError::Encoding(format!(
            "node slot mapping without record row: {id}"
        ))),
    }
}
/// Direct slot-keyed EDGES fetch — mirrors `read_node_by_slot`. Used by the
/// traversal read path (`read.rs`), which already has the edge's slot from
/// an adjacency entry and has no ULID to resolve through `EDGE_SLOTS`.
pub(crate) fn read_edge_by_slot(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
    scopes: &ScopeRegistry,
    node_ids: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<EdgeRecord>, TopoError> {
    let k = slot_key(slot);
    match table.get(k.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(v) => {
            let raw = crate::codec::unframe_value(v.value())?;
            let disk = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(crate::disk::edge_from_disk_v3(
                disk, dicts, scopes, node_ids,
            )?))
        }
    }
}
/// ULID-keyed EDGES fetch, same two-cause miss split as `read_node` (via
/// EDGE_SLOTS/EDGES): no mapping is `Ok(None)` ordinary not-found, a mapping
/// whose slot has no record row is `Encoding` corruption. Resolves
/// `from`/`to` back to ULIDs via `node_ids`.
fn read_edge(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
    scopes: &ScopeRegistry,
    edge_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    node_ids: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: EdgeId,
) -> Result<Option<EdgeRecord>, TopoError> {
    let Some(slot) = crate::slots::edge_slot(edge_slots, id)? else {
        return Ok(None);
    };
    match read_edge_by_slot(table, dicts, scopes, node_ids, slot)? {
        Some(rec) => Ok(Some(rec)),
        None => Err(TopoError::Encoding(format!(
            "edge slot mapping without record row: {id}"
        ))),
    }
}
/// Writes `rec` under its own (already-allocated) node slot. The slot must
/// already exist by the time this is called on every call path (CreateNode
/// allocates it just above; SetNodeProps/SetEmbedding only reach here after
/// a successful `read_node`, which itself required the slot to resolve) — a
/// miss here is corruption, not a "not found" outcome.
#[allow(clippy::too_many_arguments)]
fn put_node(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    scopes: &mut ScopeRegistry,
    node_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    rec: &NodeRecord,
) -> Result<(), TopoError> {
    let slot = crate::slots::node_slot(node_slots, rec.id)?
        .ok_or_else(|| TopoError::Encoding("put_node: missing node slot".into()))?;
    let raw = postcard::to_allocvec(&crate::disk::node_to_disk_v3(
        rec,
        dict,
        dicts,
        scopes_table,
        scopes,
    )?)
    .map_err(|e| TopoError::Encoding(e.to_string()))?;
    let f = crate::codec::frame_value(raw);
    table
        .insert(slot_key(slot).as_slice(), f.as_slice())
        .map_err(storage_err)?;
    Ok(())
}
/// See `put_node`; same "slot must already exist" invariant, via `edge_slots`.
#[allow(clippy::too_many_arguments)]
fn put_edge(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    scopes: &mut ScopeRegistry,
    edge_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    node_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    rec: &EdgeRecord,
) -> Result<(), TopoError> {
    let slot = crate::slots::edge_slot(edge_slots, rec.id)?
        .ok_or_else(|| TopoError::Encoding("put_edge: missing edge slot".into()))?;
    let raw = postcard::to_allocvec(&crate::disk::edge_to_disk_v3(
        rec,
        dict,
        dicts,
        scopes_table,
        scopes,
        node_slots,
    )?)
    .map_err(|e| TopoError::Encoding(e.to_string()))?;
    let f = crate::codec::frame_value(raw);
    table
        .insert(slot_key(slot).as_slice(), f.as_slice())
        .map_err(storage_err)?;
    Ok(())
}
fn put_embedding(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    node_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
    model: &str,
    vector: &[f32],
) -> Result<(), TopoError> {
    let slot = crate::slots::node_slot(node_slots, id)?
        .ok_or_else(|| TopoError::Encoding("put_embedding: missing node slot".into()))?;
    let raw =
        postcard::to_allocvec(&(model, vector)).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let f = crate::codec::frame_value(raw);
    table
        .insert(slot_key(slot).as_slice(), f.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

/// Applies a single (already-resolved) op to the NODES/EDGES tables,
/// validating against the current table state — which, mid-batch, already
/// reflects every earlier op in the same batch since we mutate the tables
/// incrementally within the one write transaction. Factored out so Task 7's
/// replay can reuse it without re-deriving the mutation logic.
#[allow(clippy::too_many_arguments)] // transactional table set is expanded incrementally by v3 dual writes.
fn apply_op(
    nodes: &mut Table<'_, &'static [u8], &'static [u8]>,
    edges: &mut Table<'_, &'static [u8], &'static [u8]>,
    embeddings: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    slot_meta: &mut Table<'_, &'static str, &'static [u8]>,
    node_slots: &mut Table<'_, &'static [u8], &'static [u8]>,
    node_ids: &mut Table<'_, &'static [u8], &'static [u8]>,
    edge_slots: &mut Table<'_, &'static [u8], &'static [u8]>,
    edge_ids: &mut Table<'_, &'static [u8], &'static [u8]>,
    out_adj: &mut Table<'_, &'static [u8], &'static [u8]>,
    in_adj: &mut Table<'_, &'static [u8], &'static [u8]>,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    scope_registry: &mut ScopeRegistry,
    op: &Op,
) -> Result<(), TopoError> {
    match op {
        Op::CreateNode {
            id,
            scope,
            label,
            props,
        } => {
            alloc_node_slot(slot_meta, node_slots, node_ids, *id)?;
            let rec = NodeRecord {
                id: *id,
                scope: *scope,
                label: label.clone(),
                props: props.clone(),
                embedding: None,
            };
            put_node(
                nodes,
                dict,
                dicts,
                scopes_table,
                scope_registry,
                node_slots,
                &rec,
            )
        }
        Op::SetNodeProps { id, props } => {
            let mut rec = read_node(nodes, embeddings, dicts, scope_registry, node_slots, *id)?
                .ok_or_else(|| {
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
            put_node(
                nodes,
                dict,
                dicts,
                scopes_table,
                scope_registry,
                node_slots,
                &rec,
            )
        }
        Op::SetEmbedding { id, model, vector } => {
            read_node(nodes, embeddings, dicts, scope_registry, node_slots, *id)?.ok_or_else(
                || TopoError::Rejected(format!("SetEmbedding: node {id:?} not found")),
            )?;
            put_embedding(embeddings, node_slots, *id, model, vector)
        }
        Op::RemoveNode { id } => {
            let removed_slot = crate::slots::node_slot(node_slots, *id)?
                .ok_or_else(|| TopoError::Rejected(format!("RemoveNode: node {id:?} not found")))?;
            let key = slot_key(removed_slot);
            let removed = nodes.remove(key.as_slice()).map_err(storage_err)?;
            if removed.is_none() {
                return Err(TopoError::Encoding(
                    "RemoveNode: node slot present but record row missing".into(),
                ));
            }

            embeddings.remove(key.as_slice()).map_err(storage_err)?;

            // Adjacency-assisted cascade (Task 10): the node's own OUT_ADJ and
            // IN_ADJ chunks under `removed_slot` ARE the incident-edge set —
            // draining them (bounded range scans, never a full `EDGES` scan)
            // both discovers every incident edge and removes this node's side
            // of the adjacency in one step. `out_entries` are edges where
            // `removed_slot` was `from` (each entry's `target` is the `to`
            // slot); `in_entries` are edges where `removed_slot` was `to`
            // (each entry's `target` is the `from` slot).
            let out_entries = adj_remove_all(out_adj, removed_slot)?;
            let in_entries = adj_remove_all(in_adj, removed_slot)?;
            remove_node_mapping(node_slots, node_ids, *id)?;

            // Cascade: for every incident edge, drop its counterpart entry in
            // the *other* endpoint's adjacency table, then remove the EDGES
            // row and the EDGE_SLOTS/EDGE_IDS mapping. Self-loops (from == to
            // == removed_slot) show up once in each of `out_entries` and
            // `in_entries`, but both sides of their adjacency were already
            // erased by the two `adj_remove_all` calls above — the
            // `target != removed_slot` guards skip the (already-gone)
            // counterpart lookup, and `removed_edge_slots` dedups the EDGES
            // row / mapping cleanup so it runs exactly once per edge.
            let mut removed_edge_slots: std::collections::HashSet<u64> =
                std::collections::HashSet::new();
            for (edge_type, entry) in out_entries {
                let to_slot = entry.target;
                let edge_slot = entry.edge;
                if to_slot != removed_slot {
                    adj_remove_edge(in_adj, to_slot, edge_type, edge_slot)?;
                }
                if removed_edge_slots.insert(edge_slot) {
                    edges
                        .remove(slot_key(edge_slot).as_slice())
                        .map_err(storage_err)?;
                    if let Some(edge_id) = crate::slots::edge_ulid(edge_ids, edge_slot)? {
                        remove_edge_mapping(edge_slots, edge_ids, edge_id)?;
                    }
                }
            }
            for (edge_type, entry) in in_entries {
                let from_slot = entry.target;
                let edge_slot = entry.edge;
                if from_slot != removed_slot {
                    adj_remove_edge(out_adj, from_slot, edge_type, edge_slot)?;
                }
                if removed_edge_slots.insert(edge_slot) {
                    edges
                        .remove(slot_key(edge_slot).as_slice())
                        .map_err(storage_err)?;
                    if let Some(edge_id) = crate::slots::edge_ulid(edge_ids, edge_slot)? {
                        remove_edge_mapping(edge_slots, edge_ids, edge_id)?;
                    }
                }
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
            let from_rec = read_node(nodes, embeddings, dicts, scope_registry, node_slots, *from)?
                .ok_or_else(|| {
                    TopoError::Rejected(format!("CreateEdge {id:?}: from node {from:?} not found"))
                })?;
            let to_rec = read_node(nodes, embeddings, dicts, scope_registry, node_slots, *to)?
                .ok_or_else(|| {
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
            let edge_slot = alloc_edge_slot(slot_meta, edge_slots, edge_ids, *id)?;
            let from_slot = crate::slots::node_slot(node_slots, *from)?
                .ok_or_else(|| TopoError::Encoding("missing from slot".into()))?;
            let to_slot = crate::slots::node_slot(node_slots, *to)?
                .ok_or_else(|| TopoError::Encoding("missing to slot".into()))?;
            let edge_type = dicts.intern(dict, DictKind::EdgeType, ty)?;
            // Intern (not `id_of`): an edge's scope can be its first
            // appearance in the file — e.g. a project-scoped edge between two
            // Shared nodes — so it may not be registered yet. Idempotent for
            // already-seen scopes; `put_edge` below re-interns the same scope
            // internally, also a no-op.
            let scope_id = scope_registry.intern(scopes_table, *scope)?;
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
            put_edge(
                edges,
                dict,
                dicts,
                scopes_table,
                scope_registry,
                edge_slots,
                node_slots,
                &rec,
            )?;
            let entry = AdjEntryDisk {
                target: to_slot,
                edge: edge_slot,
                scope: scope_id,
                valid_from: rec.valid_from,
                valid_to: None,
            };
            adj_insert(out_adj, from_slot, edge_type, entry)?;
            adj_insert(
                in_adj,
                to_slot,
                edge_type,
                AdjEntryDisk {
                    target: from_slot,
                    edge: edge_slot,
                    scope: scope_id,
                    valid_from: rec.valid_from,
                    valid_to: None,
                },
            )
        }
        Op::CloseEdge { id, valid_to } => {
            let mut rec = read_edge(edges, dicts, scope_registry, edge_slots, node_ids, *id)?
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
            put_edge(
                edges,
                dict,
                dicts,
                scopes_table,
                scope_registry,
                edge_slots,
                node_slots,
                &rec,
            )?;
            let edge_slot = crate::slots::edge_slot(edge_slots, *id)?
                .ok_or_else(|| TopoError::Encoding("missing edge slot".into()))?;
            let from_slot = crate::slots::node_slot(node_slots, rec.from)?
                .ok_or_else(|| TopoError::Encoding("missing from slot".into()))?;
            let to_slot = crate::slots::node_slot(node_slots, rec.to)?
                .ok_or_else(|| TopoError::Encoding("missing to slot".into()))?;
            let edge_type = dicts.intern(dict, DictKind::EdgeType, rec.ty.as_str())?;
            let valid_to = rec.valid_to.expect("set above");
            if !adj_close(out_adj, from_slot, edge_type, edge_slot, valid_to)?
                || !adj_close(in_adj, to_slot, edge_type, edge_slot, valid_to)?
            {
                return Err(TopoError::Encoding("adjacency missing closed edge".into()));
            }
            Ok(())
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
        assert_eq!(s.format_version().unwrap(), 3);
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
                meta.insert("format_version", 4u32.to_le_bytes().as_slice())
                    .unwrap();
            }
            tx.commit().unwrap();
        }

        // Reopening must now be rejected rather than silently accepted.
        // `.err()` drops the (non-`Debug`) `Storage` from the `Ok` arm.
        let err = Storage::open(&path).err().expect("reopen must be rejected");
        match err {
            TopoError::UnsupportedFormat {
                found: 4,
                supported: 3,
            } => {}
            other => {
                panic!("expected UnsupportedFormat {{ found: 4, supported: 3 }}, got {other:?}")
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

    /// Pins the two-cause miss split in `read_node`/`read_edge`: a
    /// NODE_SLOTS/EDGE_SLOTS mapping whose slot has NO record row is
    /// data-integrity corruption and must surface as `TopoError::Encoding`
    /// on both the read path and every write-path op that resolves the id —
    /// never as a routine `Rejected("not found")`. A ULID with no mapping at
    /// all stays ordinary not-found (`Rejected` / `Ok(None)`).
    #[test]
    fn slot_mapping_without_record_row_is_encoding_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let a = NodeId::from_u128(1);
        let b = NodeId::from_u128(2);
        let e = EdgeId::from_u128(1);
        s.apply_batch(
            vec![
                Op::CreateNode {
                    id: a,
                    scope: Scope::Shared,
                    label: "M".into(),
                    props: Default::default(),
                },
                Op::CreateNode {
                    id: b,
                    scope: Scope::Shared,
                    label: "M".into(),
                    props: Default::default(),
                },
                Op::CreateEdge {
                    id: e,
                    scope: Scope::Shared,
                    ty: "T".into(),
                    from: a,
                    to: b,
                    props: Default::default(),
                    valid_from: Some(0),
                },
            ],
            0,
        )
        .unwrap();

        // Manufacture the corrupt state via raw redb writes, bypassing every
        // Storage invariant: forward slot mappings pointing at slot 999,
        // which has no NODES/EDGES row. (Forward-mapping values are u64 LE —
        // see `slots::alloc`.)
        let ghost_node = NodeId::from_u128(99);
        let ghost_edge = EdgeId::from_u128(99);
        {
            let tx = s.db.begin_write().unwrap();
            {
                let mut node_slots = tx.open_table(NODE_SLOTS).unwrap();
                node_slots
                    .insert(
                        ghost_node.as_u128().to_be_bytes().as_slice(),
                        999u64.to_le_bytes().as_slice(),
                    )
                    .unwrap();
                let mut edge_slots = tx.open_table(EDGE_SLOTS).unwrap();
                edge_slots
                    .insert(
                        ghost_edge.as_u128().to_be_bytes().as_slice(),
                        999u64.to_le_bytes().as_slice(),
                    )
                    .unwrap();
            }
            tx.commit().unwrap();
        }

        // Read path: storage-level fetches error loudly instead of Ok(None).
        assert!(
            matches!(s.load_node(ghost_node), Err(TopoError::Encoding(_))),
            "corrupt node mapping must read as Encoding"
        );
        assert!(
            matches!(s.load_edge(ghost_edge), Err(TopoError::Encoding(_))),
            "corrupt edge mapping must read as Encoding"
        );

        // Write path: ops that resolve the ghost ids report corruption, not
        // an ordinary rejection.
        let err = s
            .apply_batch(
                vec![Op::SetNodeProps {
                    id: ghost_node,
                    props: [(
                        "k".to_string(),
                        Some(crate::props::PropValue::Str("v".into())),
                    )]
                    .into(),
                }],
                0,
            )
            .unwrap_err();
        assert!(
            matches!(err, TopoError::Encoding(_)),
            "SetNodeProps on corrupt mapping must be Encoding, got {err:?}"
        );
        let err = s
            .apply_batch(
                vec![Op::CloseEdge {
                    id: ghost_edge,
                    valid_to: None,
                }],
                0,
            )
            .unwrap_err();
        assert!(
            matches!(err, TopoError::Encoding(_)),
            "CloseEdge on corrupt mapping must be Encoding, got {err:?}"
        );

        // A ULID with no mapping at all is still an ordinary not-found.
        let err = s
            .apply_batch(
                vec![Op::SetNodeProps {
                    id: NodeId::from_u128(1234),
                    props: Default::default(),
                }],
                0,
            )
            .unwrap_err();
        assert!(
            matches!(err, TopoError::Rejected(_)),
            "absent mapping must stay Rejected, got {err:?}"
        );
        assert!(s.load_node(NodeId::from_u128(1234)).unwrap().is_none());

        // The healthy rows are untouched by the corruption probes.
        assert!(s.load_node(a).unwrap().is_some());
        assert!(s.load_edge(e).unwrap().is_some());
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

    /// Task 10: `RemoveNode`'s cascade is adjacency-assisted (no `EDGES`
    /// scan). Two high-degree nodes, 500 incident edges apiece, sharing 20
    /// direct edges between them. Removing one must: erase every one of its
    /// incident edges (its own leaf edges AND the shared edges), leave the
    /// survivor's adjacency intact minus exactly the shared edges, leave the
    /// removed node with zero adjacency chunks in either direction table,
    /// leave no dangling EDGE_SLOTS/EDGE_IDS rows for its former edges, and
    /// land an exact `EDGES` row count.
    #[test]
    fn remove_node_cascades_via_adjacency_at_scale() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());

        const A_LEAVES: usize = 480;
        const B_LEAVES: usize = 480;
        const SHARED: usize = 20; // direct A -> B edges, incident to both

        let a = NodeId::new();
        let b = NodeId::new();
        let a_leaves: Vec<NodeId> = (0..A_LEAVES).map(|_| NodeId::new()).collect();
        let b_leaves: Vec<NodeId> = (0..B_LEAVES).map(|_| NodeId::new()).collect();

        let mut ops = vec![
            Op::CreateNode {
                id: a,
                scope,
                label: "Hub".into(),
                props: Default::default(),
            },
            Op::CreateNode {
                id: b,
                scope,
                label: "Hub".into(),
                props: Default::default(),
            },
        ];
        for &leaf in a_leaves.iter().chain(b_leaves.iter()) {
            ops.push(Op::CreateNode {
                id: leaf,
                scope,
                label: "Leaf".into(),
                props: Default::default(),
            });
        }
        let a_leaf_edges: Vec<EdgeId> = a_leaves
            .iter()
            .map(|&leaf| {
                let e = EdgeId::new();
                ops.push(Op::CreateEdge {
                    id: e,
                    scope,
                    ty: "REL".into(),
                    from: a,
                    to: leaf,
                    props: Default::default(),
                    valid_from: None,
                });
                e
            })
            .collect();
        let b_leaf_edges: Vec<EdgeId> = b_leaves
            .iter()
            .map(|&leaf| {
                let e = EdgeId::new();
                ops.push(Op::CreateEdge {
                    id: e,
                    scope,
                    ty: "REL".into(),
                    from: b,
                    to: leaf,
                    props: Default::default(),
                    valid_from: None,
                });
                e
            })
            .collect();
        let shared_edges: Vec<EdgeId> = (0..SHARED)
            .map(|_| {
                let e = EdgeId::new();
                ops.push(Op::CreateEdge {
                    id: e,
                    scope,
                    ty: "LINK".into(),
                    from: a,
                    to: b,
                    props: Default::default(),
                    valid_from: None,
                });
                e
            })
            .collect();

        s.apply_batch(ops, 0).unwrap();

        let edges_before = {
            let tx = s.db.begin_read().unwrap();
            let edges = tx.open_table(EDGES).unwrap();
            edges.iter().unwrap().count()
        };
        assert_eq!(edges_before, A_LEAVES + B_LEAVES + SHARED);

        // Capture raw slots before removal — `a`'s NODE_SLOTS mapping is
        // erased by `RemoveNode` itself.
        let (a_slot, b_slot) = {
            let tx = s.db.begin_read().unwrap();
            let node_slots = tx.open_table(NODE_SLOTS).unwrap();
            (
                crate::slots::node_slot(&node_slots, a).unwrap().unwrap(),
                crate::slots::node_slot(&node_slots, b).unwrap().unwrap(),
            )
        };

        s.apply_batch(vec![Op::RemoveNode { id: a }], 1).unwrap();

        // A is gone, along with every edge it was incident to.
        assert!(s.load_node(a).unwrap().is_none());
        for &e in a_leaf_edges.iter().chain(shared_edges.iter()) {
            assert!(s.load_edge(e).unwrap().is_none());
        }

        // B survives untouched save for the shared edges.
        assert!(s.load_node(b).unwrap().is_some());
        for &e in &b_leaf_edges {
            assert!(s.load_edge(e).unwrap().is_some());
        }

        // Exact EDGES row count: only B's own leaf edges remain.
        let edges_after = {
            let tx = s.db.begin_read().unwrap();
            let edges = tx.open_table(EDGES).unwrap();
            edges.iter().unwrap().count()
        };
        assert_eq!(edges_after, B_LEAVES);

        // The removed node leaves NO chunks in either adjacency table.
        {
            let tx = s.db.begin_read().unwrap();
            let out_adj = tx.open_table(OUT_ADJ).unwrap();
            let in_adj = tx.open_table(IN_ADJ).unwrap();
            assert!(crate::adj::read_adj(&out_adj, a_slot, None)
                .unwrap()
                .is_empty());
            assert!(crate::adj::read_adj(&in_adj, a_slot, None)
                .unwrap()
                .is_empty());
        }

        // No dangling EDGE_SLOTS rows for any of A's former edges.
        {
            let tx = s.db.begin_read().unwrap();
            let edge_slots = tx.open_table(EDGE_SLOTS).unwrap();
            for &e in a_leaf_edges.iter().chain(shared_edges.iter()) {
                assert!(crate::slots::edge_slot(&edge_slots, e).unwrap().is_none());
            }
        }

        // B's adjacency is intact minus exactly the shared edges: still has
        // all B_LEAVES out-edges and no trace of A; its in-adjacency (which
        // held only the shared A->B edges) is now empty.
        {
            let tx = s.db.begin_read().unwrap();
            let out_adj = tx.open_table(OUT_ADJ).unwrap();
            let in_adj = tx.open_table(IN_ADJ).unwrap();
            let out_entries = crate::adj::read_adj(&out_adj, b_slot, None).unwrap();
            assert_eq!(out_entries.len(), B_LEAVES);
            assert!(!out_entries.iter().any(|(_, e)| e.target == a_slot));
            assert!(crate::adj::read_adj(&in_adj, b_slot, None)
                .unwrap()
                .is_empty());
        }
    }

    /// A self-loop (`from == to == removed_slot`) shows up once in
    /// `out_entries` and once in `in_entries` during the cascade. Pins that
    /// the `target != removed_slot` counterpart-skip and the
    /// `removed_edge_slots` dedup don't double-remove or error on it, and
    /// that an ordinary edge sharing the removed node stays correctly
    /// cascaded alongside it.
    #[test]
    fn remove_node_cascades_a_self_loop_without_double_removal() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let (a, b, loop_edge, ab_edge) =
            (NodeId::new(), NodeId::new(), EdgeId::new(), EdgeId::new());
        s.apply_batch(
            vec![
                Op::CreateNode {
                    id: a,
                    scope,
                    label: "M".into(),
                    props: Default::default(),
                },
                Op::CreateNode {
                    id: b,
                    scope,
                    label: "M".into(),
                    props: Default::default(),
                },
                Op::CreateEdge {
                    id: loop_edge,
                    scope,
                    ty: "SELF".into(),
                    from: a,
                    to: a,
                    props: Default::default(),
                    valid_from: None,
                },
                Op::CreateEdge {
                    id: ab_edge,
                    scope,
                    ty: "REL".into(),
                    from: a,
                    to: b,
                    props: Default::default(),
                    valid_from: None,
                },
            ],
            0,
        )
        .unwrap();

        s.apply_batch(vec![Op::RemoveNode { id: a }], 1).unwrap();

        assert!(s.load_node(a).unwrap().is_none());
        assert!(s.load_node(b).unwrap().is_some());
        assert!(s.load_edge(loop_edge).unwrap().is_none());
        assert!(s.load_edge(ab_edge).unwrap().is_none());

        // Exactly the self-loop and the a->b edge are gone; nothing else was
        // ever created, so EDGES must be empty.
        let edges_after = {
            let tx = s.db.begin_read().unwrap();
            let edges = tx.open_table(EDGES).unwrap();
            edges.iter().unwrap().count()
        };
        assert_eq!(edges_after, 0);
    }

    /// I5 regression: a `rebuild_state_from_ops` that fails mid-transaction
    /// (a corrupt tail op in the log) must not leave the in-memory
    /// `dicts`/`scope_registry` mirrors cleared — `dicts.clear()` and the
    /// scope-registry reload both run BEFORE the ops replay that can itself
    /// fail, so on error both mirrors must be reloaded from the last
    /// COMMITTED rows (the transaction itself aborts cleanly on disk; only
    /// the in-memory mirrors were at risk of drifting).
    #[test]
    fn rebuild_error_reloads_dicts_and_scope_registry_mirrors_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let a = NodeId::new();
        let mut props = crate::props::Props::new();
        props.insert(
            "name".to_string(),
            crate::props::PropValue::Str("ada".into()),
        );
        s.apply_batch(
            vec![Op::CreateNode {
                id: a,
                scope,
                label: "Person".into(),
                props,
            }],
            0,
        )
        .unwrap();

        // Manufacture a corrupt tail op directly in the log via the raw
        // append seam (bypassing `apply_batch`'s own validation): a
        // `RemoveNode` targeting a ULID that was never created. `apply_op`
        // rejects this during replay — exactly the mid-transaction failure
        // `rebuild_state_from_ops` must recover the mirrors from.
        s.append_ops(&[Op::RemoveNode { id: NodeId::new() }])
            .unwrap();

        let err = s.rebuild_state_from_ops().unwrap_err();
        assert!(
            matches!(err, TopoError::Rejected(_)),
            "expected Rejected, got {err:?}"
        );

        // The failed rebuild must not have left the mirrors cleared: a plain
        // read of the pre-existing node must still resolve its label/scope
        // correctly off the RELOADED mirrors.
        let rec = s
            .load_node(a)
            .unwrap()
            .expect("node must still be readable after a failed rebuild");
        assert_eq!(rec.label, "Person");
        assert_eq!(rec.scope, scope);
        assert_eq!(
            rec.props.get("name"),
            Some(&crate::props::PropValue::Str("ada".into()))
        );

        // The mirrors must also be usable for a WRITE, not just a read —
        // proves `dicts`/`scope_registry` are fully reloaded, not merely
        // non-panicking.
        s.apply_batch(
            vec![Op::CreateNode {
                id: NodeId::new(),
                scope,
                label: "Person".into(),
                props: Default::default(),
            }],
            1,
        )
        .expect("storage must remain writable after a failed rebuild");
    }
}
