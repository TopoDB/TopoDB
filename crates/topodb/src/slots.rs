//! Monotonic dense internal slots and ULID↔slot mapping tables.
use crate::error::{storage_err, TopoError};
use crate::ids::{EdgeId, NodeId};
use redb::{ReadableTable, Table, TableDefinition};

pub(crate) const NODE_SLOTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("node_slots");
pub(crate) const NODE_IDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("node_ids");
pub(crate) const EDGE_SLOTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edge_slots");
pub(crate) const EDGE_IDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edge_ids");
fn id_key(id: u128) -> [u8; 16] {
    id.to_be_bytes()
}
pub(crate) fn slot_key(slot: u64) -> [u8; 8] {
    slot.to_be_bytes()
}
fn read_slot(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: u128,
) -> Result<Option<u64>, TopoError> {
    match table.get(id_key(id).as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(value) => {
            let bytes: [u8; 8] = value
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad slot value".into()))?;
            Ok(Some(u64::from_le_bytes(bytes)))
        }
    }
}
fn read_id(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<u128>, TopoError> {
    match table.get(slot_key(slot).as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(value) => {
            let bytes: [u8; 16] = value
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad slot id".into()))?;
            Ok(Some(u128::from_be_bytes(bytes)))
        }
    }
}
fn alloc(
    meta: &mut Table<'_, &'static str, &'static [u8]>,
    forward: &mut Table<'_, &'static [u8], &'static [u8]>,
    reverse: &mut Table<'_, &'static [u8], &'static [u8]>,
    counter: &str,
    id: u128,
) -> Result<u64, TopoError> {
    if let Some(slot) = read_slot(forward, id)? {
        return Ok(slot);
    }
    let slot = match meta.get(counter).map_err(storage_err)? {
        None => 0,
        Some(value) => {
            let b: [u8; 8] = value
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad slot counter".into()))?;
            u64::from_le_bytes(b)
        }
    };
    let next = slot
        .checked_add(1)
        .ok_or_else(|| TopoError::Encoding("slot space exhausted".into()))?;
    forward
        .insert(id_key(id).as_slice(), slot.to_le_bytes().as_slice())
        .map_err(storage_err)?;
    reverse
        .insert(slot_key(slot).as_slice(), id_key(id).as_slice())
        .map_err(storage_err)?;
    meta.insert(counter, next.to_le_bytes().as_slice())
        .map_err(storage_err)?;
    Ok(slot)
}
pub(crate) fn alloc_node_slot(
    meta: &mut Table<'_, &'static str, &'static [u8]>,
    slots: &mut Table<'_, &'static [u8], &'static [u8]>,
    ids: &mut Table<'_, &'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<u64, TopoError> {
    alloc(meta, slots, ids, "next_node_slot", id.as_u128())
}
pub(crate) fn alloc_edge_slot(
    meta: &mut Table<'_, &'static str, &'static [u8]>,
    slots: &mut Table<'_, &'static [u8], &'static [u8]>,
    ids: &mut Table<'_, &'static [u8], &'static [u8]>,
    id: EdgeId,
) -> Result<u64, TopoError> {
    alloc(meta, slots, ids, "next_edge_slot", id.as_u128())
}
pub(crate) fn node_slot(
    t: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<Option<u64>, TopoError> {
    read_slot(t, id.as_u128())
}
pub(crate) fn edge_slot(
    t: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: EdgeId,
) -> Result<Option<u64>, TopoError> {
    read_slot(t, id.as_u128())
}
pub(crate) fn node_ulid(
    t: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<NodeId>, TopoError> {
    Ok(read_id(t, slot)?.map(NodeId::from_u128))
}
pub(crate) fn edge_ulid(
    t: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<EdgeId>, TopoError> {
    Ok(read_id(t, slot)?.map(EdgeId::from_u128))
}
fn remove(
    forward: &mut Table<'_, &'static [u8], &'static [u8]>,
    reverse: &mut Table<'_, &'static [u8], &'static [u8]>,
    id: u128,
) -> Result<(), TopoError> {
    if let Some(slot) = read_slot(forward, id)? {
        forward.remove(id_key(id).as_slice()).map_err(storage_err)?;
        reverse
            .remove(slot_key(slot).as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}
pub(crate) fn remove_node_mapping(
    forward: &mut Table<'_, &'static [u8], &'static [u8]>,
    reverse: &mut Table<'_, &'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<(), TopoError> {
    remove(forward, reverse, id.as_u128())
}
pub(crate) fn remove_edge_mapping(
    forward: &mut Table<'_, &'static [u8], &'static [u8]>,
    reverse: &mut Table<'_, &'static [u8], &'static [u8]>,
    id: EdgeId,
) -> Result<(), TopoError> {
    remove(forward, reverse, id.as_u128())
}
#[cfg(test)]
mod tests {
    use super::*;
    use redb::Database;
    #[test]
    fn allocation_is_monotonic_and_bidirectional() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut m = tx.open_table(crate::storage::META).unwrap();
            let mut ns = tx.open_table(NODE_SLOTS).unwrap();
            let mut ni = tx.open_table(NODE_IDS).unwrap();
            let mut es = tx.open_table(EDGE_SLOTS).unwrap();
            let mut ei = tx.open_table(EDGE_IDS).unwrap();
            let a = NodeId::from_u128(1);
            let b = NodeId::from_u128(2);
            assert_eq!(alloc_node_slot(&mut m, &mut ns, &mut ni, a).unwrap(), 0);
            assert_eq!(alloc_node_slot(&mut m, &mut ns, &mut ni, b).unwrap(), 1);
            assert_eq!(
                alloc_edge_slot(&mut m, &mut es, &mut ei, EdgeId::from_u128(1)).unwrap(),
                0
            );
            remove_node_mapping(&mut ns, &mut ni, a).unwrap();
            assert_eq!(
                alloc_node_slot(&mut m, &mut ns, &mut ni, NodeId::from_u128(3)).unwrap(),
                2
            );
            assert_eq!(node_ulid(&ni, 1).unwrap(), Some(b));
            assert_eq!(node_slot(&ns, a).unwrap(), None);
        }
        tx.commit().unwrap();
    }
}
