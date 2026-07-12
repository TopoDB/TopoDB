//! Groundwork for the v2 -> v3 migration.
//!
//! These frozen decode types intentionally mirror the committed v2 on-disk row
//! layout, so the chained migration tests can read a real v2 file even after
//! the live disk structs move on.

use crate::adj::{adj_insert, AdjEntryDisk};
use crate::codec::unframe_value;
use crate::dict::{DictKind, Dicts};
use crate::error::{storage_err, TopoError};
use crate::fts::{doc_text, fts_update};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::prop_index::index_node;
use crate::props::PropValue;
use crate::scopes::{seed_shared, ScopeRegistry};
use crate::slots::{alloc_edge_slot, alloc_node_slot, node_slot};
use crate::state::{EdgeRecord, NodeRecord};
#[cfg(test)]
use crate::storage::{EDGES, META, NODES};
use redb::{ReadableTable, Table};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodeRecordDiskV2 {
    pub id: NodeId,
    pub scope: Scope,
    pub label: u32,
    pub props: BTreeMap<u32, PropValue>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EdgeRecordDiskV2 {
    pub id: EdgeId,
    pub scope: Scope,
    pub ty: u32,
    pub from: NodeId,
    pub to: NodeId,
    pub props: BTreeMap<u32, PropValue>,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}

pub(crate) fn decode_v2_node(bytes: &[u8], dicts: &Dicts) -> Result<NodeRecord, TopoError> {
    let raw = unframe_value(bytes)?;
    let disk: NodeRecordDiskV2 =
        postcard::from_bytes(raw.as_ref()).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let mut props = crate::props::Props::new();
    for (key, value) in disk.props {
        props.insert(dicts.resolve(DictKind::PropKey, key)?.to_string(), value);
    }
    Ok(NodeRecord {
        id: disk.id,
        scope: disk.scope,
        label: dicts.resolve(DictKind::Label, disk.label)?,
        props,
        embedding: None,
    })
}

pub(crate) fn decode_v2_edge(bytes: &[u8], dicts: &Dicts) -> Result<EdgeRecord, TopoError> {
    let raw = unframe_value(bytes)?;
    let disk: EdgeRecordDiskV2 =
        postcard::from_bytes(raw.as_ref()).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let mut props = crate::props::Props::new();
    for (key, value) in disk.props {
        props.insert(dicts.resolve(DictKind::PropKey, key)?.to_string(), value);
    }
    Ok(EdgeRecord {
        id: disk.id,
        scope: disk.scope,
        ty: dicts.resolve(DictKind::EdgeType, disk.ty)?,
        from: disk.from,
        to: disk.to,
        props,
        valid_from: disk.valid_from,
        valid_to: disk.valid_to,
    })
}

pub(crate) fn collect_v2_rows(
    nodes: &impl ReadableTable<&'static [u8], &'static [u8]>,
    edges: &impl ReadableTable<&'static [u8], &'static [u8]>,
    dicts: &Dicts,
) -> Result<(Vec<NodeRecord>, Vec<EdgeRecord>), TopoError> {
    let mut node_rows = Vec::new();
    for item in nodes.iter().map_err(storage_err)? {
        let (_, value) = item.map_err(storage_err)?;
        node_rows.push(decode_v2_node(value.value(), dicts)?);
    }
    let mut edge_rows = Vec::new();
    for item in edges.iter().map_err(storage_err)? {
        let (_, value) = item.map_err(storage_err)?;
        edge_rows.push(decode_v2_edge(value.value(), dicts)?);
    }
    Ok((node_rows, edge_rows))
}

/// v2 -> v3 migration: builds every v3 sidecar table from the committed v2
/// rows (as before) AND re-keys the four record tables (NODES, EDGES,
/// EMBEDDINGS, COUNTERS) from their v2 ULID keys into the v3 dense-slot
/// layout (v3 spec §3), assigning slots in the same v2 ULID iteration order
/// `collect_v2_rows` has always used. EMBEDDINGS/COUNTERS rows are snapshotted
/// by their old ULID key BEFORE any table is drained, then re-inserted under
/// each node's freshly-assigned slot key — a node with no pre-existing
/// embedding/counter row simply gets none in v3 either.
///
/// Also re-keys the three FTS tables (POSTINGS/FTS_DOCS/FTS_STATS, v3 spec
/// §3 FTS rows): a v2 file's postings are `scope_key(scope) ++ term` keyed
/// with ULID-scoped ids and postcard `Vec<(NodeId, u32)>` values, FTS_DOCS is
/// ULID-node-keyed, and FTS_STATS is `scope_key(scope)` keyed — none of which
/// `fts.rs` can read post-migration (it expects `scope_id` (u32, interned)
/// and dense node slots). Rather than transcode those old rows in place, this
/// drains all three tables and rebuilds them via `fts_update` — the SAME
/// function `apply_batch`/`rebuild_state_from_ops`/`ensure_index_spec`'s own
/// reindex use — called once per node with `(None, new_text)`, right in the
/// per-node loop below where the node's freshly-assigned slot and freshly-
/// interned scope id are already on hand. Because `fts_update`'s postings
/// encoding is a canonical, order-independent function of "which terms does
/// this node's final text contain, at what frequency" (a `BTreeMap` rebuilt
/// per term), the result here is byte-identical to what incremental
/// maintenance would produce for the same final node set, in any order —
/// this is not a separate reindex algorithm, it's the identical building
/// block invoked once per node.
#[allow(clippy::too_many_arguments)]
pub(crate) fn migrate_v2_to_v3(
    spec: Arc<crate::index::IndexSpec>,
    meta: &mut Table<'_, &'static str, &'static [u8]>,
    nodes: &mut Table<'_, &'static [u8], &'static [u8]>,
    edges: &mut Table<'_, &'static [u8], &'static [u8]>,
    embeddings: &mut Table<'_, &'static [u8], &'static [u8]>,
    counters: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict_table: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    node_slots: &mut Table<'_, &'static [u8], &'static [u8]>,
    node_ids: &mut Table<'_, &'static [u8], &'static [u8]>,
    edge_slots: &mut Table<'_, &'static [u8], &'static [u8]>,
    edge_ids: &mut Table<'_, &'static [u8], &'static [u8]>,
    out_adj: &mut Table<'_, &'static [u8], &'static [u8]>,
    in_adj: &mut Table<'_, &'static [u8], &'static [u8]>,
    prop_index: &mut Table<'_, &'static [u8], &'static [u8]>,
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    docs: &mut Table<'_, &'static [u8], &'static [u8]>,
    stats: &mut Table<'_, &'static [u8], &'static [u8]>,
    vector_dims: &mut Table<'_, &'static [u8], &'static [u8]>,
    vectors: &mut Table<'_, &'static [u8], &'static [u8]>,
    embedding_ref: &mut Table<'_, &'static [u8], &'static [u8]>,
) -> Result<(), TopoError> {
    let (node_rows, edge_rows) = collect_v2_rows(nodes, edges, dicts)?;

    // Snapshot the old ULID-keyed EMBEDDINGS/COUNTERS rows before anything is
    // drained — both tables are still v2-keyed (by node ULID) at this point.
    let mut old_embeddings = std::collections::HashMap::new();
    for item in embeddings.iter().map_err(storage_err)? {
        let (k, v) = item.map_err(storage_err)?;
        old_embeddings.insert(k.value().to_vec(), v.value().to_vec());
    }
    let mut old_counters = std::collections::HashMap::new();
    for item in counters.iter().map_err(storage_err)? {
        let (k, v) = item.map_err(storage_err)?;
        old_counters.insert(k.value().to_vec(), v.value().to_vec());
    }

    nodes.retain(|_, _| false).map_err(storage_err)?;
    edges.retain(|_, _| false).map_err(storage_err)?;
    embeddings.retain(|_, _| false).map_err(storage_err)?;
    counters.retain(|_, _| false).map_err(storage_err)?;
    node_slots.retain(|_, _| false).map_err(storage_err)?;
    node_ids.retain(|_, _| false).map_err(storage_err)?;
    edge_slots.retain(|_, _| false).map_err(storage_err)?;
    edge_ids.retain(|_, _| false).map_err(storage_err)?;
    out_adj.retain(|_, _| false).map_err(storage_err)?;
    in_adj.retain(|_, _| false).map_err(storage_err)?;
    prop_index.retain(|_, _| false).map_err(storage_err)?;
    // Old (v2, pre-W2b) FTS rows are byte-incompatible with the v3 layout —
    // see the function doc comment. Rebuilt below, in the per-node loop.
    postings.retain(|_, _| false).map_err(storage_err)?;
    docs.retain(|_, _| false).map_err(storage_err)?;
    stats.retain(|_, _| false).map_err(storage_err)?;
    scopes_table.retain(|_, _| false).map_err(storage_err)?;
    seed_shared(scopes_table)?;
    meta.remove("next_node_slot").map_err(storage_err)?;
    meta.remove("next_edge_slot").map_err(storage_err)?;
    let mut scopes = ScopeRegistry::load_table_for_rebuild(scopes_table)?;
    for node in &node_rows {
        alloc_node_slot(meta, node_slots, node_ids, node.id)?;
        let slot = node_slot(node_slots, node.id)?
            .ok_or_else(|| TopoError::Encoding("missing migrated node slot".into()))?;
        index_node(prop_index, &spec, dicts, node, slot)?;

        let disk_node =
            crate::disk::node_to_disk_v3(node, dict_table, dicts, scopes_table, &mut scopes)?;
        // `node_to_disk_v3` just interned `node.scope` (idempotent past the
        // first node in this scope) — reuse that same id for the FTS rebuild
        // below rather than resolving it a second time.
        let scope_id = disk_node.scope;
        let raw =
            postcard::to_allocvec(&disk_node).map_err(|e| TopoError::Encoding(e.to_string()))?;
        let framed = crate::codec::frame_value(raw);
        nodes
            .insert(crate::storage::slot_key(slot).as_slice(), framed.as_slice())
            .map_err(storage_err)?;

        // Rebuild this node's FTS rows in the target v3 layout — see the
        // function doc comment for why this (not a byte-transcode of the old
        // rows) is the migration route.
        let new_text = doc_text(&spec, node);
        fts_update(
            postings,
            docs,
            stats,
            scope_id,
            slot,
            None,
            new_text.as_deref(),
        )?;

        let old_key = crate::storage::node_key(node.id);
        if let Some(bytes) = old_embeddings.get(old_key.as_slice()) {
            embeddings
                .insert(crate::storage::slot_key(slot).as_slice(), bytes.as_slice())
                .map_err(storage_err)?;
            // v4 dual-write (Task 5 fix): the old `embeddings` row above is
            // byte-identical to what `put_embedding` writes on the live path
            // — `frame_value(postcard(model: String, vector: Vec<f32>))` —
            // so it decodes the same way `read_embedding` does. Without this,
            // a migrated file's embeddings would exist only in the
            // still-authoritative v3 `embeddings` table and never in the v4
            // `vectors`/`embedding_ref` tables `Db::search_vector` now reads
            // exclusively (see `tests/format_fixture.rs`, which caught this:
            // the old RAM-slab read path didn't care, since it scanned
            // `embeddings` directly at open time regardless of migration
            // provenance). Mirrors `apply_op`'s `SetEmbedding` arm exactly:
            // intern the model name, pin/check its dim, then dual-write.
            let raw = crate::codec::unframe_value(bytes)?;
            let (model, vector): (String, Vec<f32>) = postcard::from_bytes(raw.as_ref())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            let model_id = dicts.intern(dict_table, DictKind::Model, &model)?;
            crate::storage::check_or_pin_dim(vector_dims, model_id, vector.len()).map_err(|e| {
                match e {
                    TopoError::Rejected(msg) => TopoError::Rejected(format!(
                        "migrating embedding for model {model:?}: {msg}"
                    )),
                    other => other,
                }
            })?;
            crate::vector_store::put_vector(
                vectors,
                embedding_ref,
                model_id,
                scope_id,
                slot,
                &vector,
            )?;
        }
        if let Some(bytes) = old_counters.get(old_key.as_slice()) {
            counters
                .insert(crate::storage::slot_key(slot).as_slice(), bytes.as_slice())
                .map_err(storage_err)?;
        }
    }
    for edge in &edge_rows {
        let edge_slot = alloc_edge_slot(meta, edge_slots, edge_ids, edge.id)?;
        let from_slot = node_slot(node_slots, edge.from)?
            .ok_or_else(|| TopoError::Encoding("missing migrated from slot".into()))?;
        let to_slot = node_slot(node_slots, edge.to)?
            .ok_or_else(|| TopoError::Encoding("missing migrated to slot".into()))?;
        let edge_type = dicts
            .id_of(DictKind::EdgeType, edge.ty.as_str())
            .ok_or_else(|| TopoError::Encoding("missing migrated edge type id".into()))?;
        let scope_id = scopes.intern(scopes_table, edge.scope)?;
        adj_insert(
            out_adj,
            from_slot,
            edge_type,
            AdjEntryDisk {
                target: to_slot,
                edge: edge_slot,
                scope: scope_id,
                valid_from: edge.valid_from,
                valid_to: edge.valid_to,
            },
        )?;
        adj_insert(
            in_adj,
            to_slot,
            edge_type,
            AdjEntryDisk {
                target: from_slot,
                edge: edge_slot,
                scope: scope_id,
                valid_from: edge.valid_from,
                valid_to: edge.valid_to,
            },
        )?;

        let raw = postcard::to_allocvec(&crate::disk::edge_to_disk_v3(
            edge,
            dict_table,
            dicts,
            scopes_table,
            &mut scopes,
            node_slots,
        )?)
        .map_err(|e| TopoError::Encoding(e.to_string()))?;
        let framed = crate::codec::frame_value(raw);
        edges
            .insert(
                crate::storage::slot_key(edge_slot).as_slice(),
                framed.as_slice(),
            )
            .map_err(storage_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adj::{IN_ADJ, OUT_ADJ};
    use crate::dict::Dicts;
    use crate::index::{IndexSpec, PropIndex};
    use crate::prop_index::PROP_INDEX;
    use crate::scopes::SCOPES;
    use crate::slots::{EDGE_IDS, EDGE_SLOTS, NODE_IDS, NODE_SLOTS};
    use redb::Database;

    #[test]
    fn frozen_v2_decoders_read_the_workload_fixture() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/v2-workload.redb");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.redb");
        std::fs::copy(source, &path).unwrap();

        let db = Database::open(&path).unwrap();
        let tx = db.begin_read().unwrap();
        let dicts = Dicts::load(&tx).unwrap();
        let nodes = tx.open_table(NODES).unwrap();
        let edges = tx.open_table(EDGES).unwrap();
        let (nodes, edges) = collect_v2_rows(&nodes, &edges, &dicts).unwrap();

        assert_eq!(nodes.len(), 240);
        assert!(nodes.iter().any(|node| node.label == "Memory"));
        assert!(!edges.is_empty());
        assert!(edges.iter().all(|edge| edge.valid_from > 0));
    }

    #[test]
    fn sidecar_migration_populates_slot_and_adjacency_tables() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/v2-workload.redb");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.redb");
        std::fs::copy(source, &path).unwrap();
        let db = Database::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        let mut dicts = {
            let read = db.begin_read().unwrap();
            Dicts::load(&read).unwrap()
        };
        {
            let mut meta = tx.open_table(META).unwrap();
            let mut nodes = tx.open_table(NODES).unwrap();
            let mut edges = tx.open_table(EDGES).unwrap();
            let mut embeddings = tx.open_table(crate::storage::EMBEDDINGS).unwrap();
            let mut counters = tx.open_table(crate::storage::COUNTERS).unwrap();
            let mut scopes = tx.open_table(SCOPES).unwrap();
            let mut node_slots = tx.open_table(NODE_SLOTS).unwrap();
            let mut node_ids = tx.open_table(NODE_IDS).unwrap();
            let mut edge_slots = tx.open_table(EDGE_SLOTS).unwrap();
            let mut edge_ids = tx.open_table(EDGE_IDS).unwrap();
            let mut out_adj = tx.open_table(OUT_ADJ).unwrap();
            let mut in_adj = tx.open_table(IN_ADJ).unwrap();
            let mut prop_index = tx.open_table(PROP_INDEX).unwrap();
            let mut dict_table = tx.open_table(crate::dict::DICT).unwrap();
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            let mut docs = tx.open_table(crate::storage::FTS_DOCS).unwrap();
            let mut stats = tx.open_table(crate::storage::FTS_STATS).unwrap();
            let mut vector_dims = tx.open_table(crate::storage::VECTOR_DIMS).unwrap();
            let mut vectors = tx.open_table(crate::vector_store::VECTORS).unwrap();
            let mut embedding_ref = tx.open_table(crate::vector_store::EMBEDDING_REF).unwrap();
            migrate_v2_to_v3(
                Arc::new(IndexSpec {
                    equality: vec![PropIndex {
                        label: "Entity".into(),
                        prop: "name".into(),
                    }],
                    text: vec![PropIndex {
                        label: "Memory".into(),
                        prop: "content".into(),
                    }],
                }),
                &mut meta,
                &mut nodes,
                &mut edges,
                &mut embeddings,
                &mut counters,
                &mut dict_table,
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
                &mut vector_dims,
                &mut vectors,
                &mut embedding_ref,
            )
            .unwrap();
            assert!(node_ids.iter().unwrap().next().is_some());
            assert!(edge_ids.iter().unwrap().next().is_some());
            assert!(out_adj.iter().unwrap().next().is_some());
            assert!(prop_index.iter().unwrap().next().is_some());
            // W2b: the v2 fixture declares a `Memory.content` text index, so
            // migration must have rebuilt real postings, not just left the
            // tables empty.
            assert!(
                postings.iter().unwrap().next().is_some(),
                "migrate_v2_to_v3 must rebuild POSTINGS in the v3 scope-id/slot layout"
            );
            assert!(
                docs.iter().unwrap().next().is_some(),
                "migrate_v2_to_v3 must rebuild FTS_DOCS in the v3 slot-keyed layout"
            );
            assert!(
                stats.iter().unwrap().next().is_some(),
                "migrate_v2_to_v3 must rebuild FTS_STATS in the v3 scope-id-keyed layout"
            );
        }
    }

    /// Writes a v2-shaped file directly (no v1 step, no pre-built fixture
    /// binary): `nodes`/`edges` are encoded through the SAME frozen v2
    /// encoders (`disk::node_to_disk`/`edge_to_disk`) `migrate.rs`'s v1->v2
    /// step uses, keyed by v2 ULID keys, then META `"format_version"` is
    /// stamped `2`. The caller reopens via `Storage::open`/`open_with` to
    /// exercise the REAL migration cutover in `Storage::open_with_options`
    /// (the `Some(2)` arm), not `migrate_v2_to_v3` called directly.
    fn write_v2_fixture(path: &std::path::Path, nodes: &[NodeRecord], edges: &[EdgeRecord]) {
        let db = Database::create(path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut dict_table = tx.open_table(crate::dict::DICT).unwrap();
            let mut dicts = Dicts::default();
            {
                let mut nodes_table = tx.open_table(NODES).unwrap();
                for n in nodes {
                    let disk = crate::disk::node_to_disk(n, &mut dict_table, &mut dicts).unwrap();
                    let raw = postcard::to_allocvec(&disk).unwrap();
                    let framed = crate::codec::frame_value(raw);
                    nodes_table
                        .insert(crate::storage::node_key(n.id).as_slice(), framed.as_slice())
                        .unwrap();
                }
            }
            {
                let mut edges_table = tx.open_table(EDGES).unwrap();
                for e in edges {
                    let disk = crate::disk::edge_to_disk(e, &mut dict_table, &mut dicts).unwrap();
                    let raw = postcard::to_allocvec(&disk).unwrap();
                    let framed = crate::codec::frame_value(raw);
                    edges_table
                        .insert(e.id.as_u128().to_be_bytes().as_slice(), framed.as_slice())
                        .unwrap();
                }
            }
            let mut meta = tx.open_table(META).unwrap();
            meta.insert("format_version", 2u32.to_le_bytes().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
    }

    /// I1 regression, sharpened: counter preservation across
    /// `rebuild_state_from_ops` must be keyed by node IDENTITY (ULID), not
    /// slot number. On a pure-v3 database replay reassigns the SAME slots
    /// (both migration-free creation and replay run in op order), so no
    /// slot divergence ever occurs and a slot-keyed "preserve by leaving the
    /// rows alone" strategy passes any test built there. The divergence is
    /// real only on a MIGRATED v2 file: `migrate_v2_to_v3` assigns slots in
    /// v2 NODES ULID-iteration order, while replay assigns them in OP-LOG
    /// order.
    ///
    /// Construction: node A gets the numerically LARGER ULID (300) but is
    /// created FIRST in the op log; node B gets the smaller ULID (100) and
    /// is created second. Migration slot order: B=0, A=1 (ULID order).
    /// Replay slot order: A=0, B=1 (op order) — guaranteed swap, asserted
    /// explicitly below so the premise can't silently rot. Pre-fix
    /// (COUNTERS untouched across rebuild), A's stats land on B and vice
    /// versa; post-fix each counter follows its ULID.
    #[test]
    fn rebuild_after_migration_keeps_counters_with_their_ulid_when_slots_diverge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let a = NodeId::from_u128(300); // created first, sorts second
        let b = NodeId::from_u128(100); // created second, sorts first
        let node = |id: NodeId| NodeRecord {
            id,
            scope: Scope::Shared,
            label: "M".into(),
            props: Default::default(),
            embedding: None,
        };
        write_v2_fixture(&path, &[node(a), node(b)], &[]);

        // The v2 op log records creation order A then B — the order replay
        // will assign slots in. (Migration never touches OPS.)
        {
            let db = Database::open(&path).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut ops_table = tx.open_table(crate::storage::OPS).unwrap();
                for (seq, id) in [(1u64, a), (2u64, b)] {
                    let op = crate::op::Op::CreateNode {
                        id,
                        scope: Scope::Shared,
                        label: "M".into(),
                        props: Default::default(),
                    };
                    ops_table
                        .insert(seq, postcard::to_allocvec(&op).unwrap().as_slice())
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }

        // Open -> v2->v3 migration assigns slots in ULID order: B=0, A=1.
        let s = crate::storage::Storage::open(&path).unwrap();
        assert_eq!(s.format_version().unwrap(), 3);
        let slot_of = |s: &crate::storage::Storage, id| {
            let tx = s.db.begin_read().unwrap();
            let t = tx.open_table(NODE_SLOTS).unwrap();
            crate::slots::node_slot(&t, id).unwrap().unwrap()
        };
        assert_eq!(
            slot_of(&s, b),
            0,
            "migration must slot B first (ULID order)"
        );
        assert_eq!(slot_of(&s, a), 1);

        // Distinct per-node stats, written synchronously via the same seam
        // the async bumper drains into.
        s.merge_counter_bumps(&[(a, 1, 10), (b, 3, 20)]).unwrap();

        s.rebuild_state_from_ops().unwrap();

        // Replay reassigned slots in op order — the divergence this test
        // exists to exercise. If these ever stop swapping, the test's
        // premise is gone and it must be rebuilt, not weakened.
        assert_eq!(slot_of(&s, a), 0, "replay must slot A first (op order)");
        assert_eq!(slot_of(&s, b), 1);

        // Each counter must have followed its ULID, not its old slot number.
        let a_stats = s.read_counter(a).unwrap().unwrap();
        let b_stats = s.read_counter(b).unwrap().unwrap();
        assert_eq!(
            (a_stats.access_count, a_stats.last_accessed_at),
            (1, 10),
            "A's stats must follow A's ULID across the rebuild"
        );
        assert_eq!(
            (b_stats.access_count, b_stats.last_accessed_at),
            (3, 20),
            "B's stats must follow B's ULID across the rebuild"
        );
    }

    /// M6 edge case: an EMPTY v2 file (no nodes, no edges — just the
    /// format_version stamp) must still migrate cleanly rather than tripping
    /// on an empty NODES iteration or similar. Every table stays empty
    /// except META (which always carries format_version/index_spec) and
    /// SCOPES (which always seeds exactly the Shared row).
    #[test]
    fn empty_v2_file_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        write_v2_fixture(&path, &[], &[]);

        let s = crate::storage::Storage::open(&path).unwrap();
        assert_eq!(s.format_version().unwrap(), 3);
        for report in s.storage_report().unwrap() {
            match report.table {
                "meta" => assert!(report.rows >= 1, "meta must retain format_version"),
                "scopes" => assert_eq!(report.rows, 1, "only the seeded Shared scope"),
                other => assert_eq!(
                    report.rows, 0,
                    "table {other} must be empty after migrating an empty v2 file"
                ),
            }
        }
    }

    /// M6 edge case: nodes exist but there are zero edges — the edge loop
    /// (adjacency insertion, edge slot allocation) must handle "nothing to
    /// do" without erroring, while the node migration still runs normally.
    #[test]
    fn v2_file_with_nodes_and_zero_edges_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let id = NodeId::new();
        let mut props = crate::props::Props::new();
        props.insert("name".to_string(), PropValue::Str("ada".into()));
        let node = NodeRecord {
            id,
            scope: Scope::Shared,
            label: "Entity".into(),
            props,
            embedding: None,
        };
        write_v2_fixture(&path, &[node], &[]);

        let s = crate::storage::Storage::open(&path).unwrap();
        assert_eq!(s.format_version().unwrap(), 3);
        let rec = s
            .load_node(id)
            .unwrap()
            .expect("migrated node must be readable");
        assert_eq!(rec.label, "Entity");
        assert_eq!(rec.props.get("name"), Some(&PropValue::Str("ada".into())));
        let edge_rows = s
            .storage_report()
            .unwrap()
            .into_iter()
            .find(|r| r.table == "edges")
            .unwrap()
            .rows;
        assert_eq!(edge_rows, 0);
    }

    /// M6 edge case: a v2 file whose op log was already compacted
    /// (`oldest_seq > 1` in META, so OPS holds only the surviving tail)
    /// migrates cleanly, and — since the v2->v3 migration only touches
    /// NODES/EDGES/EMBEDDINGS/COUNTERS/DICT/SCOPES and the v3 sidecar
    /// tables, never OPS or META `"oldest_seq"` — both survive byte-for-byte
    /// untouched.
    #[test]
    fn v2_file_with_compacted_op_log_migrates_ops_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.redb");
        let id = NodeId::new();
        write_v2_fixture(&path, &[], &[]);

        // Simulate a compaction that dropped seqs 1..4: OPS holds only the
        // surviving tail (5, 6), and META "oldest_seq" records the floor.
        {
            let db = Database::open(&path).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut ops_table = tx.open_table(crate::storage::OPS).unwrap();
                let create = crate::op::Op::CreateNode {
                    id,
                    scope: Scope::Shared,
                    label: "Entity".into(),
                    props: Default::default(),
                };
                let set_props = crate::op::Op::SetNodeProps {
                    id,
                    props: Default::default(),
                };
                ops_table
                    .insert(5u64, postcard::to_allocvec(&create).unwrap().as_slice())
                    .unwrap();
                ops_table
                    .insert(6u64, postcard::to_allocvec(&set_props).unwrap().as_slice())
                    .unwrap();
                let mut meta = tx.open_table(META).unwrap();
                meta.insert("oldest_seq", 5u64.to_le_bytes().as_slice())
                    .unwrap();
            }
            tx.commit().unwrap();
        }

        let s = crate::storage::Storage::open(&path).unwrap();
        assert_eq!(s.format_version().unwrap(), 3);
        assert_eq!(
            s.oldest_seq().unwrap(),
            5,
            "compaction floor must survive migration untouched"
        );
        let replay = s.read_ops(5).unwrap();
        assert_eq!(
            replay.len(),
            2,
            "op log rows must survive migration untouched"
        );
        assert_eq!(replay[0].0, 5);
        assert_eq!(replay[1].0, 6);
    }
}
