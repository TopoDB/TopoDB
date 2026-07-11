//! Groundwork for the v2 -> v3 migration.
//!
//! These frozen decode types intentionally mirror the committed v2 on-disk row
//! layout, so the chained migration tests can read a real v2 file even after
//! the live disk structs move on.

use crate::codec::unframe_value;
use crate::dict::Dicts;
use crate::disk::{edge_from_disk, node_from_disk};
use crate::error::{storage_err, TopoError};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::props::PropValue;
use crate::state::{EdgeRecord, NodeRecord};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    node_from_disk(
        crate::disk::NodeRecordDisk {
            id: disk.id,
            scope: disk.scope,
            label: disk.label,
            props: disk.props,
        },
        dicts,
    )
}

pub(crate) fn decode_v2_edge(bytes: &[u8], dicts: &Dicts) -> Result<EdgeRecord, TopoError> {
    let raw = unframe_value(bytes)?;
    let disk: EdgeRecordDiskV2 =
        postcard::from_bytes(raw.as_ref()).map_err(|e| TopoError::Encoding(e.to_string()))?;
    edge_from_disk(
        crate::disk::EdgeRecordDisk {
            id: disk.id,
            scope: disk.scope,
            ty: disk.ty,
            from: disk.from,
            to: disk.to,
            props: disk.props,
            valid_from: disk.valid_from,
            valid_to: disk.valid_to,
        },
        dicts,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dicts;
    use crate::storage::{EDGES, NODES};
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
}
