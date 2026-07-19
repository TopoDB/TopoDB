//! One-shot v1 to v2 migration. The v1 types are frozen copies of the old rows.
use crate::codec::frame_value;
use crate::dict::{Dicts, InternJournal};
use crate::error::{storage_err, TopoError};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::props::Props;
use crate::state::{EdgeRecord, NodeRecord};
use redb::{ReadableTable, Table};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodeRecordV1 {
    pub id: NodeId,
    pub scope: Scope,
    pub label: SmolStr,
    pub props: Props,
    pub embedding: Option<(String, Vec<f32>)>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EdgeRecordV1 {
    pub id: EdgeId,
    pub scope: Scope,
    pub ty: SmolStr,
    pub from: NodeId,
    pub to: NodeId,
    pub props: Props,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}

pub(crate) fn migrate_v1_to_v2(
    nodes: &mut Table<'_, &'static [u8], &'static [u8]>,
    edges: &mut Table<'_, &'static [u8], &'static [u8]>,
    embeddings: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict_table: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
) -> Result<(), TopoError> {
    // `dicts` is a fresh, migration-local `Dicts::default()` (see the sole
    // caller in `storage.rs`), never the live write-path mirror, so there
    // is nothing to revert on failure — this journal exists only to satisfy
    // `intern`'s signature.
    let mut journal = InternJournal::default();
    let vals = nodes
        .iter()
        .map_err(storage_err)?
        .map(|x| x.map(|(_, v)| v.value().to_vec()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_err)?;
    for b in vals {
        let old: NodeRecordV1 =
            postcard::from_bytes(&b).map_err(|e| TopoError::Encoding(e.to_string()))?;
        if let Some((m, v)) = &old.embedding {
            let raw =
                postcard::to_allocvec(&(m, v)).map_err(|e| TopoError::Encoding(e.to_string()))?;
            let framed = frame_value(raw);
            embeddings
                .insert(
                    crate::storage::node_key(old.id).as_slice(),
                    framed.as_slice(),
                )
                .map_err(storage_err)?;
        }
        let n = NodeRecord {
            id: old.id,
            scope: old.scope,
            label: old.label,
            props: old.props,
            embedding: None,
        };
        let raw = postcard::to_allocvec(&crate::disk::node_to_disk(
            &n,
            dict_table,
            dicts,
            &mut journal,
        )?)
        .map_err(|e| TopoError::Encoding(e.to_string()))?;
        let f = frame_value(raw);
        nodes
            .insert(crate::storage::node_key(n.id).as_slice(), f.as_slice())
            .map_err(storage_err)?;
    }
    let vals = edges
        .iter()
        .map_err(storage_err)?
        .map(|x| x.map(|(_, v)| v.value().to_vec()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_err)?;
    for b in vals {
        let old: EdgeRecordV1 =
            postcard::from_bytes(&b).map_err(|e| TopoError::Encoding(e.to_string()))?;
        let e = EdgeRecord {
            id: old.id,
            scope: old.scope,
            ty: old.ty,
            from: old.from,
            to: old.to,
            props: old.props,
            valid_from: old.valid_from,
            valid_to: old.valid_to,
        };
        let raw = postcard::to_allocvec(&crate::disk::edge_to_disk(
            &e,
            dict_table,
            dicts,
            &mut journal,
        )?)
        .map_err(|e| TopoError::Encoding(e.to_string()))?;
        let f = frame_value(raw);
        edges
            .insert(e.id.as_u128().to_be_bytes().as_slice(), f.as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}
