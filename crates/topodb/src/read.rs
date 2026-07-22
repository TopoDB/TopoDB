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
                if !(entry.valid_from <= t && entry.valid_to.is_none_or(|vt| t < vt)) {
                    continue;
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
}
