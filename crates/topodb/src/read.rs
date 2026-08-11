//! Scoped reads: point lookup, label scan, and k-hop temporal traversal.
//! Every entry point here takes a `&ScopeSet` (directly, or via
//! `TraversalQuery::scopes`) — there is no unscoped read path.

use crate::adj::{read_adj, IN_ADJ, OUT_ADJ};
use crate::db::Db;
use crate::dict::DictKind;
use crate::error::{storage_err, TopoError};
use crate::ids::{NodeId, ScopeSet};
use crate::props::PropValue;
use crate::slots::{node_slot, NODE_IDS, NODE_SLOTS};
use crate::state::{EdgeRecord, NodeRecord};
use crate::storage::{read_edge_by_slot, read_node_by_slot, EDGES, NODES};
use crate::vector_store::{EMBEDDING_REF, VECTORS};
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

/// Which time axis a temporal read gates on. `Valid` (the default) is
/// world/valid-time — `valid_from`/`valid_to`, identical to every predicate
/// this engine had before bi-temporal edges. `Recorded` is belief time —
/// `recorded_at`/`superseded_at` — what we had WRITTEN by `t`, regardless of
/// what the world was doing; a late-recorded fact is invisible on this axis
/// until the write actually happened, even if its `valid_from` predates `t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeAxis {
    #[default]
    Valid,
    Recorded,
}

/// Pragmatic Allen-relation subset over the half-open edge valid interval
/// `[valid_from, valid_to)` — an open edge (`valid_to = None`) is unbounded
/// on the right. Query intervals are half-open `[from, until)` too, matching
/// the temporal rewriter's `between` convention. Valid axis only: recorded-
/// axis intervals are out of scope (see [`Db::traverse_interval`]'s
/// composition rules). No disk-format change — this is query-time gating
/// over the interval fields the v9 adjacency entries already carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidInterval {
    /// Edge fully contained in `[from, until)`: `from <= valid_from` and the
    /// edge is closed with `valid_to <= until`. An open edge never satisfies
    /// `During` — its right end is unknown, so containment can't hold.
    During { from: i64, until: i64 },
    /// Edge intersects `[from, until)`: `valid_from < until` and the edge is
    /// open or ends after `from` (`valid_to > from`). Half-open on both
    /// sides: an edge closing exactly at `from`, or starting exactly at
    /// `until`, does NOT overlap.
    Overlaps { from: i64, until: i64 },
    /// Edge fully over by `t`: closed with `valid_to <= t`. An open edge
    /// never satisfies `Before`.
    Before { t: i64 },
    /// Edge starting at or after `t`: `valid_from >= t`. Open edges qualify.
    After { t: i64 },
}

impl ValidInterval {
    /// `Rejected` on a non-positive timestamp (same rule as `as_of`:
    /// positive Unix ms) or an inverted/empty interval (`until <= from`,
    /// mirroring the temporal rewriter's inverted-`between` rule).
    pub fn validate(&self) -> Result<(), TopoError> {
        let check_ts = |t: i64| {
            if t <= 0 {
                return Err(TopoError::Rejected(format!(
                    "interval timestamps must be positive Unix ms, got {t}"
                )));
            }
            Ok(())
        };
        match *self {
            ValidInterval::During { from, until } | ValidInterval::Overlaps { from, until } => {
                check_ts(from)?;
                check_ts(until)?;
                if until <= from {
                    return Err(TopoError::Rejected(format!(
                        "inverted interval: until ({until}) must be after from ({from})"
                    )));
                }
            }
            ValidInterval::Before { t } | ValidInterval::After { t } => check_ts(t)?,
        }
        Ok(())
    }

    /// Folds the four optional surface parameters into at most one predicate.
    /// `Ok(None)` when all four are absent. `Err` (message suitable for
    /// surfacing verbatim) when more than one is present, when a range is
    /// inverted (`until <= from`), or when any timestamp is not a positive
    /// Unix-millisecond value.
    pub fn from_parts(
        during: Option<(i64, i64)>,
        overlaps: Option<(i64, i64)>,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<Option<ValidInterval>, String> {
        let check_ts = |t: i64| -> Result<(), String> {
            if t <= 0 {
                return Err("interval timestamps must be positive Unix-millisecond values".into());
            }
            Ok(())
        };
        let check_range = |from: i64, until: i64, name: &str| -> Result<(), String> {
            check_ts(from)?;
            check_ts(until)?;
            if until <= from {
                return Err(format!(
                    "{} range is inverted: until ({}) must be greater than from ({})",
                    name, until, from
                ));
            }
            Ok(())
        };

        let param_count = [
            during.is_some(),
            overlaps.is_some(),
            before.is_some(),
            after.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();

        if param_count > 1 {
            let mut params = Vec::new();
            if during.is_some() {
                params.push("valid_during");
            }
            if overlaps.is_some() {
                params.push("valid_overlaps");
            }
            if before.is_some() {
                params.push("valid_before");
            }
            if after.is_some() {
                params.push("valid_after");
            }
            let param_list = params.join(" and ");
            return Err(format!(
                "at most one of valid_during / valid_overlaps / valid_before / valid_after \
                 may be set (got {})",
                param_list
            ));
        }

        if let Some((from, until)) = during {
            check_range(from, until, "valid_during")?;
            return Ok(Some(ValidInterval::During { from, until }));
        }
        if let Some((from, until)) = overlaps {
            check_range(from, until, "valid_overlaps")?;
            return Ok(Some(ValidInterval::Overlaps { from, until }));
        }
        if let Some(t) = before {
            check_ts(t)?;
            return Ok(Some(ValidInterval::Before { t }));
        }
        if let Some(t) = after {
            check_ts(t)?;
            return Ok(Some(ValidInterval::After { t }));
        }

        Ok(None)
    }

    /// Does the edge interval `[valid_from, valid_to)` satisfy this
    /// predicate? Pure truth table — validation is [`Self::validate`]'s job.
    ///
    /// Empty intervals (an edge with `valid_to` ≤ `valid_from`) represent
    /// intervals that were never valid at any instant; they satisfy no
    /// predicate. This rule keeps the invariant that overlaps equal the union
    /// of as_of point queries: every instant in a non-empty interval returns
    /// the same edges, but an empty interval returns none at every instant.
    #[must_use]
    pub fn matches(&self, valid_from: i64, valid_to: Option<i64>) -> bool {
        // Empty intervals match no predicate.
        if let Some(vt) = valid_to {
            if vt <= valid_from {
                return false;
            }
        }
        match *self {
            ValidInterval::During { from, until } => {
                from <= valid_from && valid_to.is_some_and(|vt| vt <= until)
            }
            ValidInterval::Overlaps { from, until } => {
                valid_from < until && valid_to.is_none_or(|vt| vt > from)
            }
            ValidInterval::Before { t } => valid_to.is_some_and(|vt| vt <= t),
            ValidInterval::After { t } => valid_from >= t,
        }
    }
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
    /// Which time axis `as_of` gates hops on. Default `Valid` — behavior
    /// identical to every traversal before bi-temporal edges.
    pub time_axis: TimeAxis,
}

/// Result of a traversal: every in-scope seed plus everything reached,
/// deduped, with the full edge records (fetched from the EDGES table by
/// slot) for every traversed edge.
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
        let hit = self
            .storage()
            .load_node(id)
            .ok()
            .flatten()
            .filter(|node| scopes.contains(node.scope));
        if hit.is_some() {
            self.bump([id]);
        }
        hit
    }

    /// All nodes with the given `label`, restricted to `scopes`. Served by a
    /// `LABEL_INDEX` range scan per `(label, scope)` pair (F9-11 Task 8) —
    /// loads only matching rows, not a full NODES iteration.
    ///
    /// Order (pinned — the pre-Task-8 doc comment called this "unspecified,
    /// NODES table iteration order" incidentally, so this is a new,
    /// documented contract, not a behavior change any caller relied on):
    /// scopes in `ScopeSet::iter_scopes` order (`Shared` first if included,
    /// then each `ScopeId` ascending), and — within a scope — ascending by
    /// `node_id` (mint-time order). A storage read failure degrades to "no
    /// hits", mirroring `Db::node`'s `.ok()` treatment of a storage error as
    /// absence.
    #[must_use]
    pub fn nodes_by_label(&self, scopes: &ScopeSet, label: &str) -> Vec<NodeRecord> {
        let hits = self
            .storage()
            .load_nodes_by_label(scopes, label)
            .unwrap_or_default();
        self.bump(hits.iter().map(|n| n.id));
        hits
    }

    /// Same population and order as [`nodes_by_label`] but does NOT bump the
    /// access counters. For maintenance scans that sweep the whole label to
    /// inspect it rather than to recall it — a stale-memory scan reads
    /// `last_accessed_at` and would erase that very signal by bumping it, and
    /// dedup/orphan scans should not inflate the access-boost of everything they
    /// examine. A read for housekeeping is not a recall.
    #[must_use]
    pub fn nodes_by_label_unbumped(&self, scopes: &ScopeSet, label: &str) -> Vec<NodeRecord> {
        self.storage()
            .load_nodes_by_label(scopes, label)
            .unwrap_or_default()
    }

    /// Newest-first, `k`-bounded label scan: the `recent_memories` shape,
    /// served near-`O(k)` via reverse-bounded `LABEL_INDEX` scans per
    /// `(label, scope)` pair, merged across scopes by `node_id` descending
    /// (see `Storage::load_nodes_by_label_newest`). `k == 0` returns empty,
    /// same "degrade, don't error" spirit as `nodes_by_label`. A storage
    /// read failure likewise degrades to "no hits".
    #[must_use]
    pub fn nodes_by_label_newest(
        &self,
        scopes: &ScopeSet,
        label: &str,
        k: usize,
    ) -> Vec<NodeRecord> {
        let hits = self
            .storage()
            .load_nodes_by_label_newest(scopes, label, k)
            .unwrap_or_default();
        self.bump(hits.iter().map(|n| n.id));
        hits
    }

    /// Equality lookup against the declared `(label, prop)` index: counts as a
    /// recall access and bumps the access counters of all returned hits.
    /// `Rejected` if `(label, prop)` isn't declared in `spec.equality`, or if
    /// `value` is a `Float` (not equality-indexable — Floats never enter the
    /// index in the first place). Otherwise an index lookup followed by a
    /// scope filter.
    ///
    /// Exact match: the on-disk index keys are stored under
    /// `prop_index::normalize_str` (case/whitespace-folded), so the index
    /// probe over-fetches normalized variants; this method restores byte-exact
    /// semantics by post-filtering candidates on the stored prop value. Use
    /// [`Db::nodes_by_prop_normalized`] when the relaxed match is wanted (e.g.
    /// resolving an entity name an agent may have re-typed with different
    /// casing or spacing).
    pub fn nodes_by_prop(
        &self,
        scopes: &ScopeSet,
        label: &str,
        prop: &str,
        value: &PropValue,
    ) -> Result<Vec<NodeRecord>, TopoError> {
        let hits = self.nodes_by_prop_inner(scopes, label, prop, value, true)?;
        self.bump(hits.iter().map(|node| node.id));
        Ok(hits)
    }

    /// Like [`Db::nodes_by_prop`], but case- and whitespace-insensitive for
    /// `Str` values: `"drew powell"` matches a node whose stored value is
    /// `"Drew Powell"` (or `" Drew  Powell "`). Non-`Str` values behave
    /// identically to `nodes_by_prop` — normalization only affects strings.
    /// This is the dedup primitive: check it before creating an entity so a
    /// re-typed name resolves to the existing node instead of minting a
    /// duplicate.
    pub fn nodes_by_prop_normalized(
        &self,
        scopes: &ScopeSet,
        label: &str,
        prop: &str,
        value: &PropValue,
    ) -> Result<Vec<NodeRecord>, TopoError> {
        let hits = self.nodes_by_prop_inner(scopes, label, prop, value, false)?;
        self.bump(hits.iter().map(|node| node.id));
        Ok(hits)
    }

    fn nodes_by_prop_inner(
        &self,
        scopes: &ScopeSet,
        label: &str,
        prop: &str,
        value: &PropValue,
        exact: bool,
    ) -> Result<Vec<NodeRecord>, TopoError> {
        let spec = &self.storage().spec;
        if !spec
            .equality
            .iter()
            .any(|candidate| candidate.label == label && candidate.prop == prop)
        {
            return Err(TopoError::Rejected(format!(
                "({label}, {prop}) is not equality-indexed"
            )));
        }
        let Some(iv) = crate::index::IndexValue::of(value) else {
            return Err(TopoError::Rejected(
                "Float values are not equality-indexable".into(),
            ));
        };
        let dicts = self.storage().dicts.read().expect("dict lock poisoned");
        let Some(prop_key) = dicts.id_of(crate::dict::DictKind::PropKey, prop) else {
            return Ok(Vec::new());
        };
        drop(dicts);
        let candidates = self.storage().load_nodes_by_index(prop_key, &iv)?;
        let hits: Vec<NodeRecord> = candidates
            .into_iter()
            .filter(|node| node.label == label && scopes.contains(node.scope))
            .filter(|node| !exact || node.props.get(prop) == Some(value))
            .collect();
        Ok(hits)
    }

    /// Unindexed scoped scan for `min <= props[prop] <= max` over
    /// `PropValue::Float` values. O(scope size) — the decay-sweep primitive;
    /// there is no float range index (equality indexing explicitly excludes
    /// `Float`, see `IndexValue`). This is still a full iteration of the
    /// slot-keyed NODES table (one read transaction) — legitimate here
    /// because the API was always O(n) by contract — but (F9-11 Task 8) it
    /// streams via `Storage::load_nodes_by_float_range`, which decodes each
    /// row's embedding only for rows that pass the scope+range filter,
    /// instead of eagerly decoding every scanned row's embedding
    /// (`Storage::all_nodes`'s behavior) only to discard most of them. A
    /// storage read failure degrades to "no hits" (see `nodes_by_label`'s
    /// doc comment).
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
        self.storage()
            .load_nodes_by_float_range(scopes, prop, min, max)
            .unwrap_or_default()
    }

    /// Bounded (`1..=4` hops), scoped, temporal BFS from `q.seeds` over
    /// on-disk chunked adjacency (v3 spec §6). The whole walk runs inside one
    /// `begin_read` transaction — NODE_SLOTS/NODE_IDS/OUT_ADJ/IN_ADJ/NODES/
    /// EDGES/VECTORS/EMBEDDING_REF opened once, `dicts`/`scope_registry` read
    /// guards held for the duration — so the result is one consistent view.
    ///
    /// Per hop, prunes on entry-level fields FIRST — edge scope (via
    /// `ScopeRegistry::resolve` + `ScopeSet::contains`), the edge-type filter
    /// (already applied by `read_adj`'s bounded per-type scan), and the
    /// `as_of` window — and only fetches a node record for candidates that
    /// survive; the node-scope gate is applied on the fetched record. This
    /// avoids a node fetch for every adjacency entry, not just the ones that
    /// end up in the result.
    pub fn traverse(&self, q: &TraversalQuery) -> Result<Subgraph, TopoError> {
        self.traverse_inner(q, None)
    }

    /// [`Db::traverse`], hop-gated by an Allen predicate over edge valid time
    /// instead of the point-in-time `as_of` window (pragmatic subset — see
    /// [`ValidInterval`]). The predicate REPLACES the temporal gate, so the
    /// query must not carry one of its own: `q.as_of` must be `None` (a point
    /// query is a degenerate `Overlaps`; two gates would be ambiguous) and
    /// `q.time_axis` must be `Valid` (recorded-axis intervals are out of
    /// scope) — either is `Rejected`, as is an invalid interval. Gating stays
    /// on the adjacency entries' interval fields, so the walk fetches no
    /// extra records relative to a plain valid-axis traversal.
    pub fn traverse_interval(
        &self,
        q: &TraversalQuery,
        valid_interval: ValidInterval,
    ) -> Result<Subgraph, TopoError> {
        valid_interval.validate()?;
        if q.as_of.is_some() {
            return Err(TopoError::Rejected(
                "valid_interval and as_of are mutually exclusive (a point-in-time query is a \
                 degenerate Overlaps)"
                    .into(),
            ));
        }
        if q.time_axis == TimeAxis::Recorded {
            return Err(TopoError::Rejected(
                "valid_interval gates the valid axis only; time_axis must be Valid".into(),
            ));
        }
        self.traverse_inner(q, Some(valid_interval))
    }

    /// Shared walk for `traverse`/`traverse_interval`: a `Some`
    /// `valid_interval` swaps the per-hop temporal gate from the
    /// point-in-time `as_of`/`time_axis` window to the Allen predicate;
    /// `None` is byte-identical to every traversal before interval support.
    fn traverse_inner(
        &self,
        q: &TraversalQuery,
        valid_interval: Option<ValidInterval>,
    ) -> Result<Subgraph, TopoError> {
        if q.max_hops == 0 || q.max_hops > 4 {
            return Err(TopoError::Rejected(format!(
                "max_hops must be in 1..=4, got {}",
                q.max_hops
            )));
        }

        let t = q.as_of.unwrap_or_else(now_ms);
        let storage = self.storage();
        let dicts = storage.dicts.read().expect("dict lock poisoned");
        let scope_registry = storage
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");

        // An edge-type name with no dict entry has never been written, so it
        // simply drops out of the resolved filter — matching nothing, not an
        // error, and not "no filter" either (a `Some(vec![])` filter is
        // still a filter, just one that scans zero types).
        let type_filter: Option<Vec<u32>> = q.edge_types.as_ref().map(|names| {
            names
                .iter()
                .filter_map(|name| dicts.id_of(DictKind::EdgeType, name))
                .collect()
        });

        let tx = storage.db.begin_read().map_err(storage_err)?;
        let node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
        let node_ids = tx.open_table(NODE_IDS).map_err(storage_err)?;
        let out_adj = tx.open_table(OUT_ADJ).map_err(storage_err)?;
        let in_adj = tx.open_table(IN_ADJ).map_err(storage_err)?;
        let nodes = tx.open_table(NODES).map_err(storage_err)?;
        let edges = tx.open_table(EDGES).map_err(storage_err)?;
        let vectors = tx.open_table(VECTORS).map_err(storage_err)?;
        let embedding_ref = tx.open_table(EMBEDDING_REF).map_err(storage_err)?;

        // Frontier/visited/result sets are slot-keyed throughout the walk —
        // ULIDs are resolved only at the boundary (seeds in, records out).
        let mut visited: HashSet<u64> = HashSet::new();
        let mut result_edge_slots: HashSet<u64> = HashSet::new();
        let mut frontier: VecDeque<(u64, u8)> = VecDeque::new();

        for &seed in &q.seeds {
            let Some(slot) = node_slot(&node_slots, seed)? else {
                continue;
            };
            let Some(rec) = read_node_by_slot(
                &nodes,
                &vectors,
                &embedding_ref,
                &dicts,
                &scope_registry,
                slot,
            )?
            else {
                continue;
            };
            if q.scopes.contains(rec.scope) && visited.insert(slot) {
                frontier.push_back((slot, 0));
            }
        }

        while let Some((slot, hop)) = frontier.pop_front() {
            if hop >= q.max_hops {
                continue;
            }
            let mut candidates = Vec::new();
            if matches!(q.direction, Direction::Out | Direction::Both) {
                candidates.extend(read_adj(&out_adj, slot, type_filter.as_deref())?);
            }
            if matches!(q.direction, Direction::In | Direction::Both) {
                candidates.extend(read_adj(&in_adj, slot, type_filter.as_deref())?);
            }
            for (_ty, entry) in candidates {
                let entry_scope = scope_registry.resolve(entry.scope)?;
                if !q.scopes.contains(entry_scope) {
                    continue;
                }
                match valid_interval {
                    // Allen predicate: replaces the point-in-time window,
                    // still gating on the adjacency entry's interval fields
                    // (valid axis stays the fetch-free hot path).
                    Some(iv) => {
                        if !iv.matches(entry.valid_from, entry.valid_to) {
                            continue;
                        }
                    }
                    None => match q.time_axis {
                        TimeAxis::Valid => {
                            if !(entry.valid_from <= t && entry.valid_to.is_none_or(|vt| t < vt)) {
                                continue;
                            }
                        }
                        TimeAxis::Recorded => {
                            // Belief axis: not on the adjacency entry, so
                            // fetch the full record for this candidate only
                            // (Valid stays the byte-identical hot path
                            // above).
                            let Some(rec) = read_edge_by_slot(
                                &edges,
                                &dicts,
                                &scope_registry,
                                &node_ids,
                                entry.edge,
                            )?
                            else {
                                continue;
                            };
                            if !(rec.recorded_at <= t && rec.superseded_at.is_none_or(|st| st > t))
                            {
                                continue;
                            }
                        }
                    },
                }
                let Some(other) = read_node_by_slot(
                    &nodes,
                    &vectors,
                    &embedding_ref,
                    &dicts,
                    &scope_registry,
                    entry.target,
                )?
                else {
                    continue;
                };
                if !q.scopes.contains(other.scope) {
                    continue;
                }
                result_edge_slots.insert(entry.edge);
                if visited.insert(entry.target) {
                    frontier.push_back((entry.target, hop + 1));
                }
            }
        }

        let mut nodes_out = Vec::with_capacity(visited.len());
        for slot in &visited {
            if let Some(rec) = read_node_by_slot(
                &nodes,
                &vectors,
                &embedding_ref,
                &dicts,
                &scope_registry,
                *slot,
            )? {
                nodes_out.push(rec);
            }
        }
        let mut edges_out = Vec::with_capacity(result_edge_slots.len());
        for edge_slot in &result_edge_slots {
            if let Some(rec) =
                read_edge_by_slot(&edges, &dicts, &scope_registry, &node_ids, *edge_slot)?
            {
                edges_out.push(rec);
            }
        }

        let sg = Subgraph {
            nodes: nodes_out,
            edges: edges_out,
        };
        self.bump(sg.nodes.iter().map(|n| n.id));
        Ok(sg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adj::out_adj_key;
    use crate::{EdgeId, Op, Scope, ScopeId};

    /// Forces the write path to split one `(slot, edge_type)` adjacency list
    /// across ≥2 chunks — `CHUNK_SPLIT_TARGET` is 8KB, and ~850 same-type
    /// edges from one node (each entry costs roughly a dozen bytes once
    /// `valid_from` carries a real wall-clock millisecond timestamp) reliably
    /// clears it — then asserts a 1-hop `Out` traversal from that node still
    /// returns exactly hub-plus-every-leaf. This pins chunk-boundary
    /// iteration in `read_adj`'s bounded per-type range scan: a walk that
    /// silently stopped at the first chunk would under-report the leaf set.
    #[test]
    fn traversal_spans_multiple_adjacency_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let scope_id = ScopeId::new();
        let scope = Scope::Id(scope_id);
        let hub = NodeId::new();
        let leaves: Vec<NodeId> = (0..850).map(|_| NodeId::new()).collect();

        let mut create_ops = vec![Op::CreateNode {
            id: hub,
            scope,
            label: "Hub".into(),
            props: Default::default(),
        }];
        for &leaf in &leaves {
            create_ops.push(Op::CreateNode {
                id: leaf,
                scope,
                label: "Leaf".into(),
                props: Default::default(),
            });
        }
        db.submit(create_ops).unwrap();

        let edge_ops: Vec<Op> = leaves
            .iter()
            .map(|&leaf| Op::CreateEdge {
                id: EdgeId::new(),
                scope,
                ty: "LINK".into(),
                from: hub,
                to: leaf,
                props: Default::default(),
                valid_from: None,
                recorded_at: None,
            })
            .collect();
        db.submit(edge_ops).unwrap();

        // Confirm the fixture actually produced ≥2 chunks for (hub, LINK) —
        // otherwise this test would silently degrade to the single-chunk
        // case every other traversal test already covers.
        {
            let storage = db.storage();
            let tx = storage.db.begin_read().unwrap();
            let node_slots_table = tx.open_table(NODE_SLOTS).unwrap();
            let hub_slot = node_slot(&node_slots_table, hub).unwrap().unwrap();
            let edge_type = storage
                .dicts
                .read()
                .unwrap()
                .id_of(DictKind::EdgeType, "LINK")
                .unwrap();
            let out_adj_table = tx.open_table(OUT_ADJ).unwrap();
            let start = out_adj_key(hub_slot, edge_type, 0);
            let end = out_adj_key(hub_slot, edge_type, u32::MAX);
            let chunk_count = out_adj_table
                .range(start.as_slice()..=end.as_slice())
                .unwrap()
                .count();
            assert!(
                chunk_count >= 2,
                "fixture must force a chunk split; got {chunk_count} chunk(s)"
            );
        }

        let sub = db
            .traverse(&TraversalQuery {
                scopes: ScopeSet::of(&[scope_id]),
                seeds: vec![hub],
                max_hops: 1,
                edge_types: None,
                direction: Direction::Out,
                as_of: None,
                time_axis: TimeAxis::Valid,
            })
            .unwrap();

        let mut got: Vec<NodeId> = sub.nodes.iter().map(|n| n.id).collect();
        got.sort();
        let mut expected = leaves.clone();
        expected.push(hub);
        expected.sort();
        assert_eq!(
            got, expected,
            "multi-chunk traversal must return hub + every leaf"
        );
        assert_eq!(sub.edges.len(), leaves.len());
    }

    fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
        v.sort();
        v
    }

    /// Three a→b edges spanning the interval shapes the Allen predicates
    /// care about: e1 closed `[100, 200)`, e2 open `[150, ∞)`, e3 closed
    /// `[300, 400)` (backdated creates/closes via `submit_at`, so the
    /// fixture is deterministic).
    fn allen_fixture() -> (tempfile::TempDir, Db, ScopeId, NodeId, NodeId, [EdgeId; 3]) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let scope_id = ScopeId::new();
        let scope = Scope::Id(scope_id);
        let a = NodeId::new();
        let b = NodeId::new();
        let mk_node = |id| Op::CreateNode {
            id,
            scope,
            label: "Entity".into(),
            props: Default::default(),
        };
        db.submit(vec![mk_node(a), mk_node(b)]).unwrap();
        let edges = [EdgeId::new(), EdgeId::new(), EdgeId::new()];
        let mk_edge = |id, valid_from| Op::CreateEdge {
            id,
            scope,
            ty: "LINK".into(),
            from: a,
            to: b,
            props: Default::default(),
            valid_from: Some(valid_from),
            recorded_at: None,
        };
        db.submit_at(
            vec![
                mk_edge(edges[0], 100),
                mk_edge(edges[1], 150),
                mk_edge(edges[2], 300),
            ],
            300,
        )
        .unwrap();
        db.submit_at(
            vec![
                Op::CloseEdge {
                    id: edges[0],
                    valid_to: Some(200),
                    superseded_at: None,
                },
                Op::CloseEdge {
                    id: edges[2],
                    valid_to: Some(400),
                    superseded_at: None,
                },
            ],
            400,
        )
        .unwrap();
        (dir, db, scope_id, a, b, edges)
    }

    /// Every predicate × {closed edge, open edge} × the half-open boundary
    /// values, straight off the spec's truth table.
    #[test]
    fn allen_predicate_truth_table_closed_and_open_edges() {
        use ValidInterval::{After, Before, During, Overlaps};
        // During [100, 200): containment; both bounds inclusive-of-touching
        // because edge and query intervals are both half-open.
        assert!(During {
            from: 100,
            until: 200
        }
        .matches(100, Some(200)));
        assert!(During {
            from: 100,
            until: 200
        }
        .matches(150, Some(180)));
        assert!(!During {
            from: 100,
            until: 200
        }
        .matches(99, Some(150)));
        assert!(!During {
            from: 100,
            until: 200
        }
        .matches(150, Some(201)));
        assert!(
            !During {
                from: 100,
                until: 200
            }
            .matches(150, None),
            "an open edge never satisfies a finite During"
        );
        // Overlaps [100, 200): strict at both query bounds.
        assert!(Overlaps {
            from: 100,
            until: 200
        }
        .matches(150, Some(250)));
        assert!(Overlaps {
            from: 100,
            until: 200
        }
        .matches(50, Some(101)));
        assert!(
            !Overlaps {
                from: 100,
                until: 200
            }
            .matches(50, Some(100)),
            "valid_to == from fails: Overlaps requires valid_to > from"
        );
        assert!(
            !Overlaps {
                from: 100,
                until: 200
            }
            .matches(200, Some(300)),
            "valid_from == until fails: Overlaps requires valid_from < until"
        );
        assert!(Overlaps {
            from: 100,
            until: 200
        }
        .matches(199, None));
        assert!(Overlaps {
            from: 100,
            until: 200
        }
        .matches(50, None));
        assert!(!Overlaps {
            from: 100,
            until: 200
        }
        .matches(200, None));
        // Before t: fully over by t; an open edge never satisfies Before.
        assert!(
            Before { t: 200 }.matches(100, Some(200)),
            "valid_to == t is over by t (half-open edge interval)"
        );
        assert!(!Before { t: 200 }.matches(100, Some(201)));
        assert!(!Before { t: 200 }.matches(100, None));
        // After t: starts at or after t; open edges qualify.
        assert!(After { t: 200 }.matches(200, None));
        assert!(After { t: 200 }.matches(250, Some(300)));
        assert!(!After { t: 200 }.matches(199, Some(300)));
    }

    #[test]
    fn allen_inverted_or_nonpositive_intervals_rejected() {
        use ValidInterval::{After, Before, During, Overlaps};
        for bad in [
            During {
                from: 200,
                until: 100,
            },
            During {
                from: 100,
                until: 100,
            }, // empty [a, a) counts as inverted
            Overlaps {
                from: 200,
                until: 100,
            },
            During {
                from: 0,
                until: 100,
            },
            Overlaps {
                from: -5,
                until: 100,
            },
            Before { t: 0 },
            After { t: -1 },
        ] {
            assert!(
                matches!(bad.validate(), Err(TopoError::Rejected(_))),
                "{bad:?} must be rejected"
            );
        }
        During { from: 1, until: 2 }.validate().unwrap();
        Before { t: 1 }.validate().unwrap();
        After { t: 1 }.validate().unwrap();
    }

    #[test]
    fn allen_interval_gates_edges_from_and_edges_to() {
        let (_dir, db, scope_id, a, b, [e1, e2, e3]) = allen_fixture();
        let scopes = ScopeSet::of(&[scope_id]);
        let from_ids = |iv| {
            db.edges_from_interval(&scopes, a, None, None, iv)
                .unwrap()
                .into_iter()
                .map(|e| e.id)
                .collect::<Vec<_>>()
        };
        // `edges_from_interval` returns id-sorted results, so expectations
        // are sorted the same way.
        assert_eq!(
            from_ids(ValidInterval::During {
                from: 50,
                until: 250
            }),
            sorted(vec![e1])
        );
        assert_eq!(
            from_ids(ValidInterval::During {
                from: 50,
                until: 450
            }),
            sorted(vec![e1, e3]),
            "open e2 never satisfies During"
        );
        assert_eq!(
            from_ids(ValidInterval::Overlaps {
                from: 200,
                until: 300
            }),
            sorted(vec![e2]),
            "half-open boundaries: e1 closes AT from, e3 starts AT until"
        );
        assert_eq!(
            from_ids(ValidInterval::Overlaps {
                from: 150,
                until: 350
            }),
            sorted(vec![e1, e2, e3])
        );
        assert_eq!(
            from_ids(ValidInterval::Before { t: 200 }),
            sorted(vec![e1]),
            "valid_to == t is over by t; open e2 is never Before"
        );
        assert_eq!(
            from_ids(ValidInterval::After { t: 150 }),
            sorted(vec![e2, e3])
        );
        // Reverse adjacency sees the same edges.
        let to_ids: Vec<EdgeId> = db
            .edges_to_interval(
                &scopes,
                b,
                None,
                None,
                ValidInterval::Overlaps {
                    from: 200,
                    until: 300,
                },
            )
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(to_ids, sorted(vec![e2]));
        // Target/type filters compose with the predicate; an unknown type
        // matches nothing.
        assert_eq!(
            db.edges_from_interval(
                &scopes,
                a,
                Some(b),
                Some("LINK"),
                ValidInterval::Overlaps {
                    from: 150,
                    until: 350
                },
            )
            .unwrap()
            .len(),
            3
        );
        assert!(db
            .edges_from_interval(
                &scopes,
                a,
                None,
                Some("nope"),
                ValidInterval::Overlaps {
                    from: 150,
                    until: 350
                },
            )
            .unwrap()
            .is_empty());
        // Out-of-scope read sees nothing.
        assert!(db
            .edges_from_interval(
                &ScopeSet::of(&[ScopeId::new()]),
                a,
                None,
                None,
                ValidInterval::After { t: 1 },
            )
            .unwrap()
            .is_empty());
        // The predicate validates before any read.
        assert!(matches!(
            db.edges_from_interval(&scopes, a, None, None, ValidInterval::Before { t: 0 }),
            Err(TopoError::Rejected(_))
        ));
        assert!(matches!(
            db.edges_to_interval(
                &scopes,
                b,
                None,
                None,
                ValidInterval::During { from: 9, until: 9 },
            ),
            Err(TopoError::Rejected(_))
        ));
    }

    #[test]
    fn allen_interval_gates_traverse_hops() {
        // a --[100, 200)--> b --[500, ∞)--> c
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let scope_id = ScopeId::new();
        let scope = Scope::Id(scope_id);
        let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
        let mk_node = |id| Op::CreateNode {
            id,
            scope,
            label: "Entity".into(),
            props: Default::default(),
        };
        db.submit(vec![mk_node(a), mk_node(b), mk_node(c)]).unwrap();
        let (e_ab, e_bc) = (EdgeId::new(), EdgeId::new());
        let mk_edge = |id, from, to, valid_from| Op::CreateEdge {
            id,
            scope,
            ty: "LINK".into(),
            from,
            to,
            props: Default::default(),
            valid_from: Some(valid_from),
            recorded_at: None,
        };
        db.submit_at(
            vec![mk_edge(e_ab, a, b, 100), mk_edge(e_bc, b, c, 500)],
            500,
        )
        .unwrap();
        db.submit_at(
            vec![Op::CloseEdge {
                id: e_ab,
                valid_to: Some(200),
                superseded_at: None,
            }],
            500,
        )
        .unwrap();

        let q = TraversalQuery {
            scopes: ScopeSet::of(&[scope_id]),
            seeds: vec![a],
            max_hops: 2,
            edge_types: None,
            direction: Direction::Out,
            as_of: None,
            time_axis: TimeAxis::Valid,
        };
        let node_ids = |sg: &Subgraph| sorted(sg.nodes.iter().map(|n| n.id).collect());

        // During [50, 250): the first hop passes; the open b→c edge never
        // satisfies a finite During, so c stays unreached.
        let sg = db
            .traverse_interval(
                &q,
                ValidInterval::During {
                    from: 50,
                    until: 250,
                },
            )
            .unwrap();
        assert_eq!(node_ids(&sg), sorted(vec![a, b]));
        assert_eq!(sg.edges.len(), 1);
        // Overlaps [150, 600): both hops pass (the open edge overlaps).
        let sg = db
            .traverse_interval(
                &q,
                ValidInterval::Overlaps {
                    from: 150,
                    until: 600,
                },
            )
            .unwrap();
        assert_eq!(node_ids(&sg), sorted(vec![a, b, c]));
        assert_eq!(sg.edges.len(), 2);
        // After 450: the first hop already fails, so c stays unreachable
        // even though b→c itself would qualify.
        let sg = db
            .traverse_interval(&q, ValidInterval::After { t: 450 })
            .unwrap();
        assert_eq!(node_ids(&sg), vec![a]);
        assert!(sg.edges.is_empty());
    }

    #[test]
    fn allen_traverse_interval_rejects_as_of_and_recorded_axis() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let base = TraversalQuery {
            scopes: ScopeSet::of(&[ScopeId::new()]),
            seeds: vec![],
            max_hops: 1,
            edge_types: None,
            direction: Direction::Out,
            as_of: None,
            time_axis: TimeAxis::Valid,
        };
        let iv = ValidInterval::Overlaps {
            from: 100,
            until: 200,
        };
        // A point query is a degenerate Overlaps — two temporal gates would
        // be ambiguous, so the combination is rejected outright.
        let with_as_of = TraversalQuery {
            as_of: Some(150),
            ..base.clone()
        };
        assert!(matches!(
            db.traverse_interval(&with_as_of, iv),
            Err(TopoError::Rejected(_))
        ));
        // Valid axis only — recorded-axis intervals are out of scope.
        let recorded = TraversalQuery {
            time_axis: TimeAxis::Recorded,
            ..base.clone()
        };
        assert!(matches!(
            db.traverse_interval(&recorded, iv),
            Err(TopoError::Rejected(_))
        ));
        // The predicate itself validates before any read.
        assert!(matches!(
            db.traverse_interval(
                &base,
                ValidInterval::During {
                    from: 200,
                    until: 100,
                },
            ),
            Err(TopoError::Rejected(_))
        ));
        // A well-formed combination proceeds (no seeds → empty subgraph).
        let sg = db.traverse_interval(&base, iv).unwrap();
        assert!(sg.nodes.is_empty() && sg.edges.is_empty());
    }

    /// Empty intervals (valid_to <= valid_from) were never valid at any
    /// instant and satisfy no predicate: not During, not Overlaps, not Before,
    /// not After. This preserves the invariant that overlaps([a, b)) equals the
    /// union of as_of point queries in [a, b).
    #[test]
    fn empty_intervals_match_no_predicates() {
        use ValidInterval::{After, Before, During, Overlaps};
        // Edge [1_000_000, 1_000_000): empty at one point.
        let empty_point = (1_000_000i64, Some(1_000_000i64));
        // Edge [2_000_000, 1_000_000): inverted/empty.
        let empty_inverted = (2_000_000i64, Some(1_000_000i64));
        // Predicates that would match if the empty rule were absent.
        let during = During {
            from: 500_000,
            until: 2_000_000,
        };
        let overlaps = Overlaps {
            from: 500_000,
            until: 2_000_000,
        };
        let before = Before { t: 2_000_000 };
        let after = After { t: 500_000 };

        // Empty-point edge matches none of the predicates.
        assert!(
            !during.matches(empty_point.0, empty_point.1),
            "empty edge [1_000_000, 1_000_000) must not match During [500_000, 2_000_000)"
        );
        assert!(
            !overlaps.matches(empty_point.0, empty_point.1),
            "empty edge [1_000_000, 1_000_000) must not match Overlaps [500_000, 2_000_000)"
        );
        assert!(
            !before.matches(empty_point.0, empty_point.1),
            "empty edge [1_000_000, 1_000_000) must not match Before 2_000_000"
        );
        assert!(
            !after.matches(empty_point.0, empty_point.1),
            "empty edge [1_000_000, 1_000_000) must not match After 500_000"
        );

        // Inverted edge matches none of the predicates.
        assert!(
            !during.matches(empty_inverted.0, empty_inverted.1),
            "empty edge [2_000_000, 1_000_000) must not match During [500_000, 2_000_000)"
        );
        assert!(
            !overlaps.matches(empty_inverted.0, empty_inverted.1),
            "empty edge [2_000_000, 1_000_000) must not match Overlaps [500_000, 2_000_000)"
        );
        assert!(
            !before.matches(empty_inverted.0, empty_inverted.1),
            "empty edge [2_000_000, 1_000_000) must not match Before 2_000_000"
        );
        assert!(
            !after.matches(empty_inverted.0, empty_inverted.1),
            "empty edge [2_000_000, 1_000_000) must not match After 500_000"
        );
    }

    #[test]
    fn from_parts_all_absent_returns_ok_none() {
        assert_eq!(ValidInterval::from_parts(None, None, None, None), Ok(None));
    }

    #[test]
    fn from_parts_single_param_succeeds() {
        use ValidInterval::{After, Before, During, Overlaps};
        assert_eq!(
            ValidInterval::from_parts(Some((100, 200)), None, None, None),
            Ok(Some(During {
                from: 100,
                until: 200
            }))
        );
        assert_eq!(
            ValidInterval::from_parts(None, Some((100, 200)), None, None),
            Ok(Some(Overlaps {
                from: 100,
                until: 200
            }))
        );
        assert_eq!(
            ValidInterval::from_parts(None, None, Some(150), None),
            Ok(Some(Before { t: 150 }))
        );
        assert_eq!(
            ValidInterval::from_parts(None, None, None, Some(150)),
            Ok(Some(After { t: 150 }))
        );
    }

    #[test]
    fn from_parts_two_params_err_names_both() {
        let result = ValidInterval::from_parts(Some((100, 200)), None, Some(150), None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("valid_during"));
        assert!(err.contains("valid_before"));
        assert!(err.contains("at most one"));

        let result = ValidInterval::from_parts(None, Some((100, 200)), None, Some(150));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("valid_overlaps"));
        assert!(err.contains("valid_after"));
        assert!(err.contains("at most one"));
    }

    #[test]
    fn from_parts_inverted_during_err() {
        let result = ValidInterval::from_parts(Some((200, 100)), None, None, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("valid_during"));
        assert!(err.contains("range is inverted"));
        assert!(err.contains("until"));
        assert!(err.contains("from"));
    }

    #[test]
    fn from_parts_inverted_overlaps_err() {
        let result = ValidInterval::from_parts(None, Some((200, 100)), None, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("valid_overlaps"));
        assert!(err.contains("range is inverted"));
    }

    #[test]
    fn from_parts_nonpositive_timestamps_err() {
        let result = ValidInterval::from_parts(Some((0, 100)), None, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("positive Unix-millisecond"));

        let result = ValidInterval::from_parts(None, Some((100, 0)), None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("positive Unix-millisecond"));

        let result = ValidInterval::from_parts(None, None, Some(-5), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("positive Unix-millisecond"));

        let result = ValidInterval::from_parts(None, None, None, Some(0));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("positive Unix-millisecond"));
    }
}
