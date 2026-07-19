//! v5 -> v6 migration: builds the derived `LABEL_INDEX` table
//! (`(label_id, scope_id, node_id) -> slot`) from a single scan of NODES.
//!
//! Unlike `migrate_v4.rs` (dual-write eras, re-encoding, key-shape
//! discrimination), this hop touches no existing table and re-encodes
//! nothing: every NODES row is already stored as `NodeRecordDiskV3`, whose
//! `label`/`scope` fields are ALREADY the interned dictionary/scope-registry
//! ids `LABEL_INDEX`'s key wants — so this function needs neither `Dicts`
//! nor `ScopeRegistry` to resolve anything, just a raw decode per row.
//! Idempotent (inserting the same key/value twice is a no-op), so callers
//! that reach it via more than one historic version arm in the same open
//! (any file whose stored version was < 5, which migrates through several
//! other steps first — see `Storage::open_with_options`'s match) can call it
//! unconditionally without special-casing "was this already built".
use crate::disk::NodeRecordDiskV3;
use crate::error::{storage_err, TopoError};
use crate::storage::{label_index_key, LABEL_INDEX, NODES};
use redb::ReadableTable;

/// Scans NODES (already at the current record layout on entry, regardless of
/// which historic `format_version` this open started from) and inserts one
/// LABEL_INDEX row per node: `(label_id, scope_id, node_id) -> slot`.
pub(crate) fn migrate_v5_to_v6(tx: &redb::WriteTransaction) -> Result<(), TopoError> {
    let nodes = tx.open_table(NODES).map_err(storage_err)?;
    let mut label_index = tx.open_table(LABEL_INDEX).map_err(storage_err)?;
    for entry in nodes.iter().map_err(storage_err)? {
        let (k, v) = entry.map_err(storage_err)?;
        let slot_bytes: [u8; 8] = k
            .value()
            .try_into()
            .map_err(|_| TopoError::Encoding("bad node slot key".into()))?;
        let slot = u64::from_be_bytes(slot_bytes);
        let raw = crate::codec::unframe_value(v.value())?;
        let disk: NodeRecordDiskV3 =
            postcard::from_bytes(raw.as_ref()).map_err(|e| TopoError::Encoding(e.to_string()))?;
        let key = label_index_key(disk.label, disk.scope, disk.id);
        label_index
            .insert(key.as_slice(), slot)
            .map_err(storage_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::{DictKind, Dicts, InternJournal};
    use crate::disk::node_to_disk_v3;
    use crate::ids::{NodeId, Scope, ScopeId};
    use crate::props::Props;
    use crate::scopes::{seed_shared, ScopeRegistry, SCOPES};
    use crate::slots::{alloc_node_slot, EDGE_IDS, EDGE_SLOTS, NODE_IDS, NODE_SLOTS};
    use crate::state::NodeRecord;
    use redb::ReadableTableMetadata;

    /// Directly proves the migration's one-scan behavior in isolation (no
    /// full `Storage::open_with_options` version-match plumbing): write two
    /// NODES rows by hand (mimicking the v5 on-disk shape, which is
    /// byte-identical to v6's — this hop adds a table, not a re-encoding),
    /// run `migrate_v5_to_v6`, and confirm LABEL_INDEX carries exactly the
    /// expected `(label, scope, node) -> slot` rows.
    #[test]
    fn migrate_v5_to_v6_builds_label_index_from_nodes_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();
        let n1 = NodeId::from_u128(1);
        let n2 = NodeId::from_u128(2);
        let scope = Scope::Id(ScopeId::from_u128(9));
        {
            let tx = db.begin_write().unwrap();
            {
                let mut nodes = tx.open_table(crate::storage::NODES).unwrap();
                let mut dict = tx.open_table(crate::dict::DICT).unwrap();
                let mut scopes = tx.open_table(SCOPES).unwrap();
                seed_shared(&mut scopes).unwrap();
                let mut scope_registry = ScopeRegistry::load_table_for_rebuild(&scopes).unwrap();
                let mut dicts = Dicts::default();
                let mut journal = InternJournal::default();
                let mut slot_meta = tx.open_table(crate::storage::META).unwrap();
                let mut node_slots = tx.open_table(NODE_SLOTS).unwrap();
                let mut node_ids = tx.open_table(NODE_IDS).unwrap();
                let _ = tx.open_table(EDGE_SLOTS).unwrap();
                let _ = tx.open_table(EDGE_IDS).unwrap();

                for (id, label) in [(n1, "Entity"), (n2, "Memory")] {
                    let slot = alloc_node_slot(&mut slot_meta, &mut node_slots, &mut node_ids, id)
                        .unwrap();
                    let rec = NodeRecord {
                        id,
                        scope,
                        label: label.into(),
                        props: Props::new(),
                        embedding: None,
                    };
                    let disk = node_to_disk_v3(
                        &rec,
                        &mut dict,
                        &mut dicts,
                        &mut scopes,
                        &mut scope_registry,
                        &mut journal,
                    )
                    .unwrap();
                    let raw = postcard::to_allocvec(&disk).unwrap();
                    let framed = crate::codec::frame_value(raw);
                    nodes
                        .insert(crate::storage::slot_key(slot).as_slice(), framed.as_slice())
                        .unwrap();
                }
            }
            migrate_v5_to_v6(&tx).unwrap();
            tx.commit().unwrap();
        }

        let tx = db.begin_read().unwrap();
        let label_index = tx.open_table(LABEL_INDEX).unwrap();
        assert_eq!(label_index.len().unwrap(), 2, "one row per node");
        let dict = tx.open_table(crate::dict::DICT).unwrap();
        let dicts = Dicts::load_from_table(&dict).unwrap();
        let scopes_table = tx.open_table(SCOPES).unwrap();
        let scope_registry = ScopeRegistry::load_table_for_rebuild(&scopes_table).unwrap();
        let label_id = dicts.id_of(DictKind::Label, "Entity").unwrap();
        let scope_id = scope_registry.id_of(scope).unwrap();
        let key = label_index_key(label_id, scope_id, n1);
        assert!(
            label_index.get(key.as_slice()).unwrap().is_some(),
            "n1's LABEL_INDEX row must exist under (Entity, scope, n1)"
        );
    }
}
