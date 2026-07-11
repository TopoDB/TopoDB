//! Interned on-disk v2 record twins; public records remain string-carrying.
use crate::dict::{DictKind, Dicts};
use crate::error::TopoError;
use crate::ids::{EdgeId, NodeId, Scope};
use crate::props::PropValue;
use crate::state::{EdgeRecord, NodeRecord};
use redb::Table;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodeRecordDisk {
    pub id: NodeId,
    pub scope: Scope,
    pub label: u32,
    pub props: BTreeMap<u32, PropValue>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EdgeRecordDisk {
    pub id: EdgeId,
    pub scope: Scope,
    pub ty: u32,
    pub from: NodeId,
    pub to: NodeId,
    pub props: BTreeMap<u32, PropValue>,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}
pub(crate) fn node_to_disk(
    r: &NodeRecord,
    t: &mut Table<'_, &'static [u8], &'static str>,
    d: &mut Dicts,
) -> Result<NodeRecordDisk, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k)?, v.clone());
    }
    Ok(NodeRecordDisk {
        id: r.id,
        scope: r.scope,
        label: d.intern(t, DictKind::Label, r.label.as_str())?,
        props: p,
    })
}
pub(crate) fn node_from_disk(r: NodeRecordDisk, d: &Dicts) -> Result<NodeRecord, TopoError> {
    let mut p = crate::props::Props::new();
    for (k, v) in r.props {
        p.insert(d.resolve(DictKind::PropKey, k)?.to_string(), v);
    }
    Ok(NodeRecord {
        id: r.id,
        scope: r.scope,
        label: d.resolve(DictKind::Label, r.label)?,
        props: p,
        embedding: None,
    })
}
pub(crate) fn edge_to_disk(
    r: &EdgeRecord,
    t: &mut Table<'_, &'static [u8], &'static str>,
    d: &mut Dicts,
) -> Result<EdgeRecordDisk, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k)?, v.clone());
    }
    Ok(EdgeRecordDisk {
        id: r.id,
        scope: r.scope,
        ty: d.intern(t, DictKind::EdgeType, r.ty.as_str())?,
        from: r.from,
        to: r.to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
    })
}
pub(crate) fn edge_from_disk(r: EdgeRecordDisk, d: &Dicts) -> Result<EdgeRecord, TopoError> {
    let mut p = crate::props::Props::new();
    for (k, v) in r.props {
        p.insert(d.resolve(DictKind::PropKey, k)?.to_string(), v);
    }
    Ok(EdgeRecord {
        id: r.id,
        scope: r.scope,
        ty: d.resolve(DictKind::EdgeType, r.ty)?,
        from: r.from,
        to: r.to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
    })
}
