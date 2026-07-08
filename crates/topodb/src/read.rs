//! Scoped reads: point lookup, label scan, and k-hop temporal traversal.
//! Every entry point here takes a `&ScopeSet` (directly, or via
//! `TraversalQuery::scopes`) — there is no unscoped read path.

use crate::db::Db;
use crate::error::TopoError;
use crate::graph::Snapshot;
use crate::ids::{EdgeId, NodeId, ScopeSet};
use crate::props::PropValue;
use crate::state::{EdgeRecord, NodeRecord};
use smol_str::SmolStr;
use std::collections::{HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which adjacency to walk from each frontier node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

/// A bounded, scoped, temporal breadth-first traversal request.
#[derive(Debug, Clone)]
pub struct TraversalQuery {
    pub scopes: ScopeSet,
    pub seeds: Vec<NodeId>,
    /// Hop budget. Must be `1..=4` — `0` or `>4` is rejected.
    pub max_hops: u8,
    /// `None` matches every edge type.
    pub edge_types: Option<Vec<SmolStr>>,
    pub direction: Direction,
    /// `None` means "now" — read once, at traversal start, from the wall
    /// clock (this is a read path; only writes must never embed wall-clock
    /// time).
    pub as_of: Option<i64>,
}

/// Result of a traversal: every in-scope seed plus everything reached,
/// deduped, with the full edge records (from the snapshot's `edges` map) for
/// every traversed edge.
#[derive(Debug, Clone, Default)]
pub struct Subgraph {
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64
}

impl Db {
    /// Point lookup, scoped: `None` both when the node doesn't exist and when
    /// it exists but is outside `scopes` — the two are indistinguishable to
    /// the caller, by design (no way to detect out-of-scope data via absence
    /// timing/shape).
    #[must_use]
    pub fn node(&self, scopes: &ScopeSet, id: NodeId) -> Option<NodeRecord> {
        let snap = self.snapshot();
        let hit = snap.nodes.get(&id).filter(|n| scopes.contains(n.scope)).cloned();
        if hit.is_some() {
            self.bump([id]);
        }
        hit
    }

    /// All nodes with the given `label`, restricted to `scopes`. Order is
    /// unspecified (snapshot iteration order).
    #[must_use]
    pub fn nodes_by_label(&self, scopes: &ScopeSet, label: &str) -> Vec<NodeRecord> {
        let snap = self.snapshot();
        let hits: Vec<NodeRecord> = snap
            .nodes
            .values()
            .filter(|n| n.label == label && scopes.contains(n.scope))
            .cloned()
            .collect();
        self.bump(hits.iter().map(|n| n.id));
        hits
    }

    /// Equality lookup against the declared `(label, prop)` index: counts as a
    /// recall access and bumps the access counters of all returned hits.
    /// `Rejected` if `(label, prop)` isn't declared in `spec.equality`, or if
    /// `value` is a `Float` (not equality-indexable — Floats never enter the
    /// index in the first place). Otherwise an index lookup followed by a
    /// scope filter.
    pub fn nodes_by_prop(
        &self,
        scopes: &ScopeSet,
        label: &str,
        prop: &str,
        value: &PropValue,
    ) -> Result<Vec<NodeRecord>, TopoError> {
        let snap = self.snapshot();
        if !snap.spec.equality.iter().any(|p| p.label == label && p.prop == prop) {
            return Err(TopoError::Rejected(format!(
                "({label}, {prop}) is not equality-indexed"
            )));
        }
        let Some(iv) = crate::index::IndexValue::of(value) else {
            return Err(TopoError::Rejected("Float values are not equality-indexable".into()));
        };
        let hits: Vec<NodeRecord> = snap
            .prop_index
            .get(&(SmolStr::new(label), prop.to_string(), iv))
            .into_iter()
            .flat_map(|set| set.iter())
            .filter_map(|id| snap.nodes.get(id))
            .filter(|n| scopes.contains(n.scope))
            .cloned()
            .collect();
        self.bump(hits.iter().map(|n| n.id));
        Ok(hits)
    }

    /// Unindexed scoped snapshot scan for `min <= props[prop] <= max` over
    /// `PropValue::Float` values. O(scope size) — the decay-sweep primitive;
    /// there is no float range index (equality indexing explicitly excludes
    /// `Float`, see `IndexValue`).
    /// Does NOT bump access counters, by design: this is the decay-sweep
    /// primitive. A sweep that bumped everything it scanned would overwrite the
    /// very recency signal (`last_accessed_at`) it exists to read.
    #[must_use]
    pub fn nodes_by_float_range(
        &self,
        scopes: &ScopeSet,
        prop: &str,
        min: f64,
        max: f64,
    ) -> Vec<NodeRecord> {
        let snap = self.snapshot();
        snap.nodes
            .values()
            .filter(|n| scopes.contains(n.scope))
            .filter(|n| matches!(n.props.get(prop), Some(PropValue::Float(f)) if *f >= min && *f <= max))
            .cloned()
            .collect()
    }

    /// Bounded (`1..=4` hops), scoped, temporal BFS from `q.seeds` over a
    /// single `snapshot()` — a consistent view for the whole traversal.
    pub fn traverse(&self, q: &TraversalQuery) -> Result<Subgraph, TopoError> {
        if q.max_hops == 0 || q.max_hops > 4 {
            return Err(TopoError::Rejected(format!(
                "max_hops must be in 1..=4, got {}",
                q.max_hops
            )));
        }

        let snap = self.snapshot();
        let t = q.as_of.unwrap_or_else(now_ms);

        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut result_edges: HashSet<EdgeId> = HashSet::new();
        let mut frontier: VecDeque<(NodeId, u8)> = VecDeque::new();

        for &seed in &q.seeds {
            if let Some(n) = snap.nodes.get(&seed) {
                if q.scopes.contains(n.scope) && visited.insert(seed) {
                    frontier.push_back((seed, 0));
                }
            }
        }

        while let Some((node, hop)) = frontier.pop_front() {
            if hop >= q.max_hops {
                continue;
            }
            for entry in adjacency_entries(&snap, node, q.direction) {
                if !edge_traversable(&q.scopes, q.edge_types.as_deref(), entry, t) {
                    continue;
                }
                let Some(other) = snap.nodes.get(&entry.other) else { continue };
                if !q.scopes.contains(other.scope) {
                    continue;
                }
                result_edges.insert(entry.edge);
                if visited.insert(entry.other) {
                    frontier.push_back((entry.other, hop + 1));
                }
            }
        }

        let nodes = visited
            .iter()
            .filter_map(|id| snap.nodes.get(id).cloned())
            .collect();
        let edges = result_edges
            .iter()
            .filter_map(|id| snap.edges.get(id).cloned())
            .collect();

        let sg = Subgraph { nodes, edges };
        self.bump(sg.nodes.iter().map(|n| n.id));
        Ok(sg)
    }
}

/// Collects the `AdjEntry`s to walk from `node` given `direction`. `Both`
/// walks `out` and `inn`; an edge reachable both ways (i.e. it appears in
/// both maps, which never happens for the *same* traversal step since one
/// side is keyed by `from` and the other by `to`) still only ever
/// contributes one `AdjEntry` per direction here — de-duplication of the
/// resulting node/edge id happens via `visited`/`result_edges` in the caller.
fn adjacency_entries(
    snap: &Snapshot,
    node: NodeId,
    direction: Direction,
) -> Vec<&crate::graph::AdjEntry> {
    let mut out = Vec::new();
    if matches!(direction, Direction::Out | Direction::Both) {
        if let Some(v) = snap.out.get(&node) {
            out.extend(v.iter());
        }
    }
    if matches!(direction, Direction::In | Direction::Both) {
        if let Some(v) = snap.inn.get(&node) {
            out.extend(v.iter());
        }
    }
    out
}

fn edge_traversable(
    scopes: &ScopeSet,
    edge_types: Option<&[SmolStr]>,
    entry: &crate::graph::AdjEntry,
    t: i64,
) -> bool {
    if !scopes.contains(entry.scope) {
        return false;
    }
    if let Some(types) = edge_types {
        if !types.iter().any(|ty| ty == &entry.ty) {
            return false;
        }
    }
    entry.valid_from <= t && entry.valid_to.is_none_or(|vt| t < vt)
}
