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
}
