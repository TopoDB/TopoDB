//! In-memory adjacency snapshot: `im::HashMap`/`im::Vector` (persistent,
//! structurally-shared) mirrors of the node/edge tables, kept in step with
//! storage by the applier thread (see `db.rs`). Readers get a cheap `Arc`
//! clone via `Db::snapshot` and never block on writers.

use crate::error::TopoError;
use crate::ids::{EdgeId, NodeId, Scope};
use crate::index::{IndexSpec, IndexValue};
use crate::op::Op;
use crate::state::{EdgeRecord, NodeRecord};
use crate::storage::Storage;
use smol_str::SmolStr;
use std::sync::Arc;

/// Key into `Snapshot::prop_index`: the declared `(label, prop)` plus the
/// indexed value.
type PropIndexKey = (SmolStr, String, IndexValue);

/// One directed adjacency edge, as seen from either endpoint. `other` is the
/// node at the far end (i.e. under `out[from]`, `other == to`; under
/// `inn[to]`, `other == from`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjEntry {
    pub edge: EdgeId,
    pub ty: SmolStr,
    pub other: NodeId,
    pub scope: Scope,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}

/// A persistent (structurally-shared) snapshot of the graph's nodes and
/// adjacency. `apply` produces a new `Snapshot` from an old one plus a batch
/// of resolved ops without a full copy — unaffected subtrees of the
/// underlying `im` structures are shared between old and new versions.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub(crate) nodes: im::HashMap<NodeId, NodeRecord>,
    pub(crate) out: im::HashMap<NodeId, im::Vector<AdjEntry>>,
    pub(crate) inn: im::HashMap<NodeId, im::Vector<AdjEntry>>,
    /// Full edge records, keyed by id. `AdjEntry`s (in `out`/`inn`) are kept
    /// lean (no `props`) for cheap traversal; this map is the source of
    /// truth for anything that needs a complete `EdgeRecord` (props
    /// included). Kept in step with `out`/`inn` by every arm below.
    pub(crate) edges: im::HashMap<EdgeId, EdgeRecord>,
    /// The index configuration this snapshot was built/maintained under.
    /// `Arc` so cloning a `Snapshot` (cheap, structural-share) never deep
    /// copies it; carried alongside the index it governs so a reader never
    /// observes `prop_index` maintained under a different spec than the one
    /// it's validating lookups against.
    pub(crate) spec: Arc<IndexSpec>,
    /// Equality index: `(label, prop, value) -> node ids with that value`.
    /// Only ever holds entries for `(label, prop)` pairs declared in
    /// `spec.equality`, and only for nodes whose value at that prop is
    /// equality-indexable (see `IndexValue::of`). Emptied `OrdSet`s are
    /// dropped rather than left behind as empty values (mirrors the
    /// `out`/`inn` empty-key lesson from Plan 1).
    pub(crate) prop_index: im::HashMap<PropIndexKey, im::OrdSet<NodeId>>,
}

/// Inserts `rec` into `prop_index` under every `(label, prop)` declared in
/// `spec.equality` that matches `rec.label` and whose value in `rec.props`
/// is present and equality-indexable. Non-declared labels/props and
/// non-indexable values (Float) are left untouched — no-ops.
fn index_node(
    prop_index: &mut im::HashMap<PropIndexKey, im::OrdSet<NodeId>>,
    spec: &IndexSpec,
    rec: &NodeRecord,
) {
    for pi in &spec.equality {
        if pi.label != rec.label {
            continue;
        }
        let Some(value) = rec.props.get(&pi.prop) else { continue };
        let Some(iv) = IndexValue::of(value) else { continue };
        prop_index.entry((pi.label.clone(), pi.prop.clone(), iv)).or_default().insert(rec.id);
    }
}

/// Inverse of `index_node`: removes `rec` from every `(label, prop, value)`
/// entry it currently occupies per `spec.equality`, dropping any `OrdSet`
/// left empty by the removal (never leaves an empty-set key behind).
fn unindex_node(
    prop_index: &mut im::HashMap<PropIndexKey, im::OrdSet<NodeId>>,
    spec: &IndexSpec,
    rec: &NodeRecord,
) {
    for pi in &spec.equality {
        if pi.label != rec.label {
            continue;
        }
        let Some(value) = rec.props.get(&pi.prop) else { continue };
        let Some(iv) = IndexValue::of(value) else { continue };
        let key = (pi.label.clone(), pi.prop.clone(), iv);
        if let Some(set) = prop_index.get_mut(&key) {
            set.remove(&rec.id);
            if set.is_empty() {
                prop_index.remove(&key);
            }
        }
    }
}

impl Snapshot {
    /// Rebuilds a snapshot from scratch by scanning storage. Used at `Db`
    /// open time (and by tests to check incremental application against a
    /// from-scratch rebuild). `spec` governs which `(label, prop)` pairs get
    /// folded into `prop_index` as nodes load.
    pub(crate) fn from_storage(storage: &Storage, spec: Arc<IndexSpec>) -> Result<Snapshot, TopoError> {
        let mut nodes = im::HashMap::new();
        for n in storage.all_nodes()? {
            nodes.insert(n.id, n);
        }

        let mut prop_index: im::HashMap<PropIndexKey, im::OrdSet<NodeId>> = im::HashMap::new();
        for n in nodes.values() {
            index_node(&mut prop_index, &spec, n);
        }

        let mut out: im::HashMap<NodeId, im::Vector<AdjEntry>> = im::HashMap::new();
        let mut inn: im::HashMap<NodeId, im::Vector<AdjEntry>> = im::HashMap::new();
        let mut edges: im::HashMap<EdgeId, EdgeRecord> = im::HashMap::new();
        for e in storage.all_edges()? {
            out.entry(e.from).or_default().push_back(AdjEntry {
                edge: e.id,
                ty: e.ty.clone(),
                other: e.to,
                scope: e.scope,
                valid_from: e.valid_from,
                valid_to: e.valid_to,
            });
            inn.entry(e.to).or_default().push_back(AdjEntry {
                edge: e.id,
                ty: e.ty.clone(),
                other: e.from,
                scope: e.scope,
                valid_from: e.valid_from,
                valid_to: e.valid_to,
            });
            edges.insert(e.id, e);
        }

        Ok(Snapshot { nodes, out, inn, edges, spec, prop_index })
    }

    /// Applies a batch of already-resolved ops (as produced by
    /// `Storage::apply_batch`) to `self`, returning a new `Snapshot`.
    /// Persistent-structure update: cloning `im::HashMap`/`im::Vector` is
    /// O(1) (they're reference-counted trees), and every mutation below
    /// (`insert`/`remove`/`push_back`/...) returns new structure that shares
    /// untouched nodes with `self` — there is no full-map rebuild here.
    ///
    /// `CloseEdge` carries only `id` and the new `valid_to`, not the edge's
    /// `from`/`to` endpoints. We resolve them from `self.edges` — the
    /// full-`EdgeRecord` map maintained in this same pass, so a same-batch
    /// `CreateEdge` → `CloseEdge` resolves correctly against the just-inserted
    /// record. No storage read is involved, so there is no error to swallow.
    #[must_use]
    pub(crate) fn apply(&self, resolved_ops: &[Op]) -> Snapshot {
        let mut nodes = self.nodes.clone();
        let mut out = self.out.clone();
        let mut inn = self.inn.clone();
        let mut edges = self.edges.clone();
        let mut prop_index = self.prop_index.clone();

        for op in resolved_ops {
            match op {
                Op::CreateNode { id, scope, label, props } => {
                    let rec = NodeRecord {
                        id: *id,
                        scope: *scope,
                        label: label.clone(),
                        props: props.clone(),
                        embedding: None,
                    };
                    index_node(&mut prop_index, &self.spec, &rec);
                    nodes.insert(*id, rec);
                }
                Op::SetNodeProps { id, props } => {
                    // Unindex under the old values first, then reindex under
                    // the new ones — simplest correct maintenance, and what
                    // the interface doc calls for ("removes the old entry...
                    // inserts the new"). Net effect for an unchanged declared
                    // prop is a no-op (same key removed then re-added).
                    if let Some(old) = nodes.get(id).cloned() {
                        unindex_node(&mut prop_index, &self.spec, &old);
                        if let Some(rec) = nodes.get_mut(id) {
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
                        }
                        if let Some(new) = nodes.get(id) {
                            index_node(&mut prop_index, &self.spec, new);
                        }
                    }
                }
                Op::SetEmbedding { id, model, vector } => {
                    if let Some(rec) = nodes.get_mut(id) {
                        rec.embedding = Some((model.clone(), vector.clone()));
                    }
                }
                Op::RemoveNode { id } => {
                    if let Some(old) = nodes.remove(id) {
                        unindex_node(&mut prop_index, &self.spec, &old);
                    }
                    // Drop this node's own adjacency lists, and purge the
                    // matching reverse entries recorded under the *other*
                    // endpoint's key in the opposite map. `from_storage`
                    // never creates a key for a node with zero edges, so an
                    // incrementally-emptied vector's key must also be
                    // removed here — otherwise `out`/`inn` diverge from a
                    // from-scratch rebuild (stale empty-vector keys) and
                    // grow unboundedly under churn. The full-record `edges`
                    // map must also lose every incident edge, mirroring
                    // storage's `RemoveNode` handling.
                    if let Some(entries) = out.remove(id) {
                        for e in entries.iter() {
                            edges.remove(&e.edge);
                            if let Some(v) = inn.get_mut(&e.other) {
                                v.retain(|x| x.edge != e.edge);
                                if v.is_empty() {
                                    inn.remove(&e.other);
                                }
                            }
                        }
                    }
                    if let Some(entries) = inn.remove(id) {
                        for e in entries.iter() {
                            edges.remove(&e.edge);
                            if let Some(v) = out.get_mut(&e.other) {
                                v.retain(|x| x.edge != e.edge);
                                if v.is_empty() {
                                    out.remove(&e.other);
                                }
                            }
                        }
                    }
                }
                Op::CreateEdge { id, scope, ty, from, to, props, valid_from } => {
                    let vf = valid_from
                        .expect("Snapshot::apply only runs on resolved ops (valid_from filled)");
                    out.entry(*from).or_default().push_back(AdjEntry {
                        edge: *id,
                        ty: ty.clone(),
                        other: *to,
                        scope: *scope,
                        valid_from: vf,
                        valid_to: None,
                    });
                    inn.entry(*to).or_default().push_back(AdjEntry {
                        edge: *id,
                        ty: ty.clone(),
                        other: *from,
                        scope: *scope,
                        valid_from: vf,
                        valid_to: None,
                    });
                    edges.insert(
                        *id,
                        EdgeRecord {
                            id: *id,
                            scope: *scope,
                            ty: ty.clone(),
                            from: *from,
                            to: *to,
                            props: props.clone(),
                            valid_from: vf,
                            valid_to: None,
                        },
                    );
                }
                Op::CloseEdge { id, valid_to } => {
                    if let Some((from, to)) = edges.get(id).map(|e| (e.from, e.to)) {
                        if let Some(v) = out.get_mut(&from) {
                            for entry in v.iter_mut() {
                                if entry.edge == *id {
                                    entry.valid_to = *valid_to;
                                }
                            }
                        }
                        if let Some(v) = inn.get_mut(&to) {
                            for entry in v.iter_mut() {
                                if entry.edge == *id {
                                    entry.valid_to = *valid_to;
                                }
                            }
                        }
                        if let Some(e) = edges.get_mut(id) {
                            e.valid_to = *valid_to;
                        }
                    }
                }
            }
        }

        Snapshot { nodes, out, inn, edges, spec: self.spec.clone(), prop_index }
    }

    #[doc(hidden)] pub fn debug_nodes(&self) -> &im::HashMap<NodeId, NodeRecord> { &self.nodes }
    #[doc(hidden)] pub fn debug_out(&self) -> &im::HashMap<NodeId, im::Vector<AdjEntry>> { &self.out }
    #[doc(hidden)] pub fn debug_inn(&self) -> &im::HashMap<NodeId, im::Vector<AdjEntry>> { &self.inn }
    #[doc(hidden)] pub fn debug_edges(&self) -> &im::HashMap<EdgeId, EdgeRecord> { &self.edges }
}

#[cfg(test)]
mod tests {
    use crate::graph::AdjEntry;
    use crate::{Db, EdgeId, NodeId, Op, Scope, ScopeId};

    #[test]
    fn incremental_snapshot_equals_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let ids: Vec<NodeId> = (0..50).map(|_| NodeId::new()).collect();
        for id in &ids {
            db.submit(vec![Op::CreateNode {
                id: *id,
                scope,
                label: "M".into(),
                props: Default::default(),
            }])
            .unwrap();
        }
        let mut edge_ids: Vec<EdgeId> = Vec::new();
        for w in ids.windows(2) {
            let e = EdgeId::new();
            edge_ids.push(e);
            db.submit(vec![Op::CreateEdge {
                id: e,
                scope,
                ty: "NEXT".into(),
                from: w[0],
                to: w[1],
                props: Default::default(),
                valid_from: None,
            }])
            .unwrap();
        }

        // Close one edge (well clear of the node we're about to remove) —
        // exercises CloseEdge's endpoint lookup and per-entry valid_to
        // update on both `out` and `inn`.
        let closed_edge = edge_ids[10]; // edge ids[10] -> ids[11]
        db.submit(vec![Op::CloseEdge { id: closed_edge, valid_to: None }]).unwrap();

        db.submit(vec![Op::RemoveNode { id: ids[25] }]).unwrap();

        let live = db.snapshot();
        // Reopen → rebuilt from storage:
        drop(db);
        let db2 = Db::open(dir.path().join("t.redb")).unwrap();
        let rebuilt = db2.snapshot();

        assert_eq!(live.nodes.len(), rebuilt.nodes.len());
        assert!(rebuilt.nodes.get(&ids[25]).is_none());

        // Full key-set equality — not per-key degree via
        // `unwrap_or_default()`, which can't distinguish "empty vector left
        // behind under this key" from "no entry for this key at all" (the
        // latter is what `from_storage` always produces for degree-0 nodes).
        let live_out_keys: std::collections::BTreeSet<NodeId> = live.out.keys().copied().collect();
        let rebuilt_out_keys: std::collections::BTreeSet<NodeId> =
            rebuilt.out.keys().copied().collect();
        assert_eq!(live_out_keys, rebuilt_out_keys, "out key-set mismatch");

        let live_inn_keys: std::collections::BTreeSet<NodeId> = live.inn.keys().copied().collect();
        let rebuilt_inn_keys: std::collections::BTreeSet<NodeId> =
            rebuilt.inn.keys().copied().collect();
        assert_eq!(live_inn_keys, rebuilt_inn_keys, "inn key-set mismatch");

        // Entry-for-entry equality (sorted by EdgeId), not just counts.
        fn sorted(v: &im::Vector<AdjEntry>) -> Vec<AdjEntry> {
            let mut v: Vec<AdjEntry> = v.iter().cloned().collect();
            v.sort_by_key(|e| e.edge);
            v
        }
        for key in &live_out_keys {
            let l = sorted(live.out.get(key).unwrap());
            let r = sorted(rebuilt.out.get(key).unwrap());
            assert_eq!(l, r, "out entries mismatch at {key:?}");
        }
        for key in &live_inn_keys {
            let l = sorted(live.inn.get(key).unwrap());
            let r = sorted(rebuilt.inn.get(key).unwrap());
            assert_eq!(l, r, "inn entries mismatch at {key:?}");
        }

        // The closed edge's `valid_to` must agree, live vs. rebuilt, at both
        // endpoints (out[from] and inn[to]).
        let (from, to) = (ids[10], ids[11]);
        let live_out_entry =
            live.out.get(&from).unwrap().iter().find(|e| e.edge == closed_edge).unwrap();
        let rebuilt_out_entry =
            rebuilt.out.get(&from).unwrap().iter().find(|e| e.edge == closed_edge).unwrap();
        assert!(live_out_entry.valid_to.is_some());
        assert_eq!(live_out_entry.valid_to, rebuilt_out_entry.valid_to);

        let live_inn_entry =
            live.inn.get(&to).unwrap().iter().find(|e| e.edge == closed_edge).unwrap();
        let rebuilt_inn_entry =
            rebuilt.inn.get(&to).unwrap().iter().find(|e| e.edge == closed_edge).unwrap();
        assert!(live_inn_entry.valid_to.is_some());
        assert_eq!(live_inn_entry.valid_to, rebuilt_inn_entry.valid_to);

        // `edges` (full EdgeRecords) must also agree, live vs. rebuilt: same
        // key-set (exercising CreateEdge inserts + RemoveNode's incident-edge
        // purge), and per-key record equality (exercising CloseEdge's
        // valid_to update landing in the full record, not just the lean
        // AdjEntry copies).
        let live_edges_keys: std::collections::BTreeSet<EdgeId> =
            live.edges.keys().copied().collect();
        let rebuilt_edges_keys: std::collections::BTreeSet<EdgeId> =
            rebuilt.edges.keys().copied().collect();
        assert_eq!(live_edges_keys, rebuilt_edges_keys, "edges key-set mismatch");
        for key in &live_edges_keys {
            let l = live.edges.get(key).unwrap();
            let r = rebuilt.edges.get(key).unwrap();
            assert_eq!(l, r, "edges record mismatch at {key:?}");
        }
    }
}
