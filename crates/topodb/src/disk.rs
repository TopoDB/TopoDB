//! Interned on-disk record twins; public records remain string-carrying.
//!
//! Two generations live here side by side:
//! - `NodeRecordDisk`/`EdgeRecordDisk` (+ `node_to_disk`/`edge_to_disk`) are
//!   the FROZEN v2 ENCODE shape: ULID-keyed rows, `scope: Scope`, ULID
//!   `from`/`to`. Kept byte-for-byte as originally written because
//!   `migrate.rs`'s v1->v2 step calls these exact functions to produce v2
//!   rows — changing them in place would silently corrupt the v1->v2->v3
//!   chain and break `migrate.rs` without touching that file. There is no
//!   corresponding `node_from_disk`/`edge_from_disk` decode pair: nothing
//!   in the live crate reads v2 rows through this frozen shape —
//!   `migrate_v3.rs` decodes v2 rows through its OWN frozen
//!   `NodeRecordDiskV2`/`EdgeRecordDiskV2` twins instead.
//! - `NodeRecordDiskV3`/`EdgeRecordDiskV3` (+ `*_v3` functions) are the LIVE
//!   v3 record-table shape (v3 spec §3): `scope` is the interned `u32`
//!   scope-registry id, and edge endpoints are `u64` node slots. These are
//!   what `storage.rs`'s NODES/EDGES read/write paths and `migrate_v3.rs`'s
//!   v2->v3 re-keying use.
use crate::dict::{DictKind, Dicts, InternJournal};
use crate::error::TopoError;
use crate::ids::{EdgeId, NodeId, Scope};
use crate::props::PropValue;
use crate::scopes::ScopeRegistry;
use crate::state::{EdgeRecord, NodeRecord};
use redb::{ReadableTable, Table};
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
    journal: &mut InternJournal,
) -> Result<NodeRecordDisk, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k, journal)?, v.clone());
    }
    Ok(NodeRecordDisk {
        id: r.id,
        scope: r.scope,
        label: d.intern(t, DictKind::Label, r.label.as_str(), journal)?,
        props: p,
    })
}
pub(crate) fn edge_to_disk(
    r: &EdgeRecord,
    t: &mut Table<'_, &'static [u8], &'static str>,
    d: &mut Dicts,
    journal: &mut InternJournal,
) -> Result<EdgeRecordDisk, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k, journal)?, v.clone());
    }
    Ok(EdgeRecordDisk {
        id: r.id,
        scope: r.scope,
        ty: d.intern(t, DictKind::EdgeType, r.ty.as_str(), journal)?,
        from: r.from,
        to: r.to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
    })
}
// ---- v3 live record-table shapes (v3 spec §3) ----

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NodeRecordDiskV3 {
    pub id: NodeId,
    pub scope: u32,
    pub label: u32,
    pub props: BTreeMap<u32, PropValue>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EdgeRecordDiskV3 {
    pub id: EdgeId,
    pub scope: u32,
    pub ty: u32,
    pub from: u64,
    pub to: u64,
    pub props: BTreeMap<u32, PropValue>,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}
/// Encodes `r` as the live v3 row: interns `r.scope` into the scope registry
/// (writing a fresh row to `scopes_table` only the first time a given scope
/// is seen) rather than requiring the caller to have pre-interned it.
pub(crate) fn node_to_disk_v3(
    r: &NodeRecord,
    t: &mut Table<'_, &'static [u8], &'static str>,
    d: &mut Dicts,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    scopes: &mut ScopeRegistry,
    journal: &mut InternJournal,
) -> Result<NodeRecordDiskV3, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k, journal)?, v.clone());
    }
    Ok(NodeRecordDiskV3 {
        id: r.id,
        scope: scopes.intern(scopes_table, r.scope, journal)?,
        label: d.intern(t, DictKind::Label, r.label.as_str(), journal)?,
        props: p,
    })
}
pub(crate) fn node_from_disk_v3(
    r: NodeRecordDiskV3,
    d: &Dicts,
    scopes: &ScopeRegistry,
) -> Result<NodeRecord, TopoError> {
    let mut p = crate::props::Props::new();
    for (k, v) in r.props {
        p.insert(d.resolve(DictKind::PropKey, k)?.to_string(), v);
    }
    Ok(NodeRecord {
        id: r.id,
        scope: scopes.resolve(r.scope)?,
        label: d.resolve(DictKind::Label, r.label)?,
        props: p,
        embedding: None,
    })
}
/// Same scope-interning behavior as `node_to_disk_v3`, plus resolves `r.from`/
/// `r.to` to their (already-allocated) node slots via `node_slots`. The
/// endpoints were validated to exist immediately before this is called on
/// every call path, so a missing slot here is `TopoError::Encoding`
/// (corruption), never `Rejected`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edge_to_disk_v3(
    r: &EdgeRecord,
    t: &mut Table<'_, &'static [u8], &'static str>,
    d: &mut Dicts,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    scopes: &mut ScopeRegistry,
    node_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    journal: &mut InternJournal,
) -> Result<EdgeRecordDiskV3, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k, journal)?, v.clone());
    }
    let from = crate::slots::node_slot(node_slots, r.from)?
        .ok_or_else(|| TopoError::Encoding("edge_to_disk_v3: missing from slot".into()))?;
    let to = crate::slots::node_slot(node_slots, r.to)?
        .ok_or_else(|| TopoError::Encoding("edge_to_disk_v3: missing to slot".into()))?;
    Ok(EdgeRecordDiskV3 {
        id: r.id,
        scope: scopes.intern(scopes_table, r.scope, journal)?,
        ty: d.intern(t, DictKind::EdgeType, r.ty.as_str(), journal)?,
        from,
        to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
    })
}
/// Resolves `r.from`/`r.to` slots back to ULIDs via `node_ids`. A miss is
/// `TopoError::Encoding` — every edge row's endpoints must have a live
/// NODE_IDS entry for as long as the edge row itself exists.
pub(crate) fn edge_from_disk_v3(
    r: EdgeRecordDiskV3,
    d: &Dicts,
    scopes: &ScopeRegistry,
    node_ids: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<EdgeRecord, TopoError> {
    let mut p = crate::props::Props::new();
    for (k, v) in r.props {
        p.insert(d.resolve(DictKind::PropKey, k)?.to_string(), v);
    }
    let from = crate::slots::node_ulid(node_ids, r.from)?
        .ok_or_else(|| TopoError::Encoding("edge_from_disk_v3: missing from ulid".into()))?;
    let to = crate::slots::node_ulid(node_ids, r.to)?
        .ok_or_else(|| TopoError::Encoding("edge_from_disk_v3: missing to ulid".into()))?;
    Ok(EdgeRecord {
        id: r.id,
        scope: scopes.resolve(r.scope)?,
        ty: d.resolve(DictKind::EdgeType, r.ty)?,
        from,
        to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
    })
}
