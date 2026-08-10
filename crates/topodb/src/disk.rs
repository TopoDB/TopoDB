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
//! - `NodeRecordDiskV3` (+ `node_to_disk_v3`/`node_from_disk_v3`) remains the
//!   LIVE node-table shape (v3 spec §3, unchanged since): `scope` is the
//!   interned `u32` scope-registry id.
//! - `EdgeRecordDiskV3` (+ `edge_to_disk_v3`) is RETAINED as an ENCODE-only
//!   shape — `migrate_v3.rs`'s v2->v3 re-keying still produces it — and as
//!   the frozen DECODE shape `migrate_v9.rs`'s v8->v9 migration reads
//!   directly (into `EdgeRecordDiskV4`, never via an intermediate
//!   `EdgeRecord`). It is no longer written by the live write path and has
//!   no live decode function of its own: `edge_from_disk_v3` was deleted
//!   once Task 2 (v9, bi-temporal edges) landed and confirmed
//!   `migrate_v9.rs` had no need for it.
//! - `EdgeRecordDiskV4` (+ `edge_to_disk_v4`/`edge_from_disk_v4`) is the LIVE
//!   v9 edge-table shape: v3 plus the belief axis (`recorded_at`,
//!   `superseded_at`), appended last (postcard is positional). This is what
//!   `storage.rs`'s EDGES read/write paths (`put_edge`/`read_edge_by_slot`/
//!   `all_edges`) use. Nodes have no belief axis, so `NodeRecordDiskV3` has
//!   no v4 twin.
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
// ---- v4 live record-table shape (belief axis: recorded_at/superseded_at) ----

/// LIVE v4 EDGES row shape: v3 (`EdgeRecordDiskV3`) plus the two belief-axis
/// fields, appended LAST — postcard is positional, so new fields must always
/// go at the end. `NodeRecordDiskV3`/`node_to_disk_v3`/`node_from_disk_v3`
/// are unaffected (nodes have no belief axis) and remain the live node
/// shape; only edges gain a v4 twin.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EdgeRecordDiskV4 {
    pub id: EdgeId,
    pub scope: u32,
    pub ty: u32,
    pub from: u64,
    pub to: u64,
    pub props: BTreeMap<u32, PropValue>,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub recorded_at: i64,
    pub superseded_at: Option<i64>,
}
/// Same scope-interning/slot-resolving behavior as `edge_to_disk_v3`, plus
/// carries the belief axis. This is what the LIVE write path
/// (`storage::put_edge`) uses; `edge_to_disk_v3` stays frozen for
/// `migrate_v3.rs`'s v2->v3 step.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edge_to_disk_v4(
    r: &EdgeRecord,
    t: &mut Table<'_, &'static [u8], &'static str>,
    d: &mut Dicts,
    scopes_table: &mut Table<'_, &'static [u8], &'static [u8]>,
    scopes: &mut ScopeRegistry,
    node_slots: &impl ReadableTable<&'static [u8], &'static [u8]>,
    journal: &mut InternJournal,
) -> Result<EdgeRecordDiskV4, TopoError> {
    let mut p = BTreeMap::new();
    for (k, v) in &r.props {
        p.insert(d.intern(t, DictKind::PropKey, k, journal)?, v.clone());
    }
    let from = crate::slots::node_slot(node_slots, r.from)?
        .ok_or_else(|| TopoError::Encoding("edge_to_disk_v4: missing from slot".into()))?;
    let to = crate::slots::node_slot(node_slots, r.to)?
        .ok_or_else(|| TopoError::Encoding("edge_to_disk_v4: missing to slot".into()))?;
    Ok(EdgeRecordDiskV4 {
        id: r.id,
        scope: scopes.intern(scopes_table, r.scope, journal)?,
        ty: d.intern(t, DictKind::EdgeType, r.ty.as_str(), journal)?,
        from,
        to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
        recorded_at: r.recorded_at,
        superseded_at: r.superseded_at,
    })
}
/// Resolves `r.from`/`r.to` slots back to ULIDs via `node_ids`, same miss
/// semantics as `node_from_disk_v3` (a missing ULID is `TopoError::Encoding`,
/// never a silent default).
pub(crate) fn edge_from_disk_v4(
    r: EdgeRecordDiskV4,
    d: &Dicts,
    scopes: &ScopeRegistry,
    node_ids: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<EdgeRecord, TopoError> {
    let mut p = crate::props::Props::new();
    for (k, v) in r.props {
        p.insert(d.resolve(DictKind::PropKey, k)?.to_string(), v);
    }
    let from = crate::slots::node_ulid(node_ids, r.from)?
        .ok_or_else(|| TopoError::Encoding("edge_from_disk_v4: missing from ulid".into()))?;
    let to = crate::slots::node_ulid(node_ids, r.to)?
        .ok_or_else(|| TopoError::Encoding("edge_from_disk_v4: missing to ulid".into()))?;
    Ok(EdgeRecord {
        id: r.id,
        scope: scopes.resolve(r.scope)?,
        ty: d.resolve(DictKind::EdgeType, r.ty)?,
        from,
        to,
        props: p,
        valid_from: r.valid_from,
        valid_to: r.valid_to,
        recorded_at: r.recorded_at,
        superseded_at: r.superseded_at,
    })
}
