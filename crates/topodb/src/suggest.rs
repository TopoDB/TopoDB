//! Link prediction: per-node missing-edge suggestions. Read-only — the
//! engine suggests, the host decides (and picks the edge type). Spec:
//! 2026-07-19 PPR & link-prediction design.

use crate::db::Db;
use crate::error::TopoError;
use crate::ids::{NodeId, ScopeSet};
use crate::ppr::{ppr_over_subgraph, SUGGEST_HOPS, WEIGHT_SEMANTIC, WEIGHT_STRUCTURAL};
use crate::read::{Direction, TraversalQuery};
use crate::recall::{leg_depth, rrf_fuse};
use crate::state::NodeRecord;
use crate::vector::VectorQuery;
use std::collections::{HashMap, HashSet};

/// A `suggest_links` request. `model` names the vector namespace for the
/// semantic leg; `None` (or no stored embedding under it) = structure-only.
/// `as_of` pins the adjacency view, `None` = now (traverse's convention).
#[derive(Debug, Clone)]
pub struct SuggestLinksQuery {
    pub scopes: ScopeSet,
    pub node: NodeId,
    pub k: usize,
    pub model: Option<String>,
    pub as_of: Option<i64>,
}

/// One suggested (but non-existent) edge endpoint, with evidence. Never a
/// typed edge — creating one, and typing it, is host/agent policy.
#[derive(Debug, Clone)]
pub struct LinkSuggestion {
    pub node: NodeRecord,
    pub score: f32,
    /// Shared 1-hop neighbors (scope/`as_of`-filtered), id-ascending.
    pub common_neighbors: Vec<NodeId>,
    /// Appeared in the PPR leg.
    pub structural: bool,
    /// Appeared in the vector leg.
    pub semantic: bool,
}

impl Db {
    /// Rank the k likeliest missing edges from `q.node`: RRF fusion of a
    /// structural leg (PPR over the SUGGEST_HOPS-bounded neighborhood) and
    /// a semantic leg (cosine against the node's own embedding under
    /// `q.model`). Self and current 1-hop neighbors are never suggested.
    /// Unknown/out-of-scope target → `Ok(empty)` (mirrors `Db::node`'s
    /// absence semantics — no existence leak).
    ///
    /// Note: the target lookup goes through `Db::node`, which bumps the
    /// node's access counter — deliberate, matching every scoped read.
    pub fn suggest_links(&self, q: &SuggestLinksQuery) -> Result<Vec<LinkSuggestion>, TopoError> {
        if q.k == 0 {
            return Err(TopoError::Rejected("suggest_links requires k > 0".into()));
        }
        // Target, scoped: absent and out-of-scope are indistinguishably
        // empty, mirroring `Db::node`.
        let Some(target) = self.node(&q.scopes, q.node) else {
            return Ok(Vec::new());
        };
        let depth = leg_depth(q.k);

        // Exclusion set: self + every node already 1-hop adjacent (any edge
        // type, in-scope, live at as_of — the traversal's own filters).
        let one_hop = self.traverse(&TraversalQuery {
            scopes: q.scopes.clone(),
            seeds: vec![q.node],
            max_hops: 1,
            edge_types: None,
            direction: Direction::Both,
            as_of: q.as_of,
        })?;
        let excluded: HashSet<NodeId> = one_hop.nodes.iter().map(|n| n.id).collect();

        // Structural leg: PPR over the 3-hop neighborhood.
        let sg = self.traverse(&TraversalQuery {
            scopes: q.scopes.clone(),
            seeds: vec![q.node],
            max_hops: SUGGEST_HOPS,
            edge_types: None,
            direction: Direction::Both,
            as_of: q.as_of,
        })?;
        let scored = ppr_over_subgraph(&sg, &[(q.node, 1.0)]);
        let mut records: HashMap<NodeId, NodeRecord> =
            sg.nodes.into_iter().map(|n| (n.id, n)).collect();
        let structural: Vec<NodeId> = scored
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| !excluded.contains(id))
            .take(depth)
            .collect();

        // Semantic leg: the target's own embedding under q.model. A missing
        // or different-model embedding is an empty leg, never an error.
        let mut semantic: Vec<NodeId> = Vec::new();
        if let Some(model) = &q.model {
            if let Some((stored_model, vector)) = &target.embedding {
                if stored_model == model {
                    let hits = self.search_vector(&VectorQuery {
                        scopes: q.scopes.clone(),
                        model: model.clone(),
                        vector: vector.clone(),
                        // Headroom so exclusions can't starve the leg.
                        k: depth + excluded.len(),
                        candidates: None,
                    })?;
                    for (n, _) in hits {
                        if semantic.len() == depth {
                            break;
                        }
                        let nid = n.id;
                        if !excluded.contains(&nid) {
                            records.entry(nid).or_insert(n);
                            semantic.push(nid);
                        }
                    }
                }
            }
        }

        let in_structural: HashSet<NodeId> = structural.iter().copied().collect();
        let in_semantic: HashSet<NodeId> = semantic.iter().copied().collect();
        let mut lists: Vec<(f32, Vec<NodeId>)> = Vec::new();
        if !structural.is_empty() {
            lists.push((WEIGHT_STRUCTURAL, structural));
        }
        if !semantic.is_empty() {
            lists.push((WEIGHT_SEMANTIC, semantic));
        }

        let mut out = Vec::new();
        for (id, score) in rrf_fuse(&lists).into_iter().take(q.k) {
            let Some(rec) = records.remove(&id) else {
                continue;
            };
            // Evidence for the k survivors only: candidate's 1-hop set
            // (same filters) intersected with the target's.
            let cand_hop = self.traverse(&TraversalQuery {
                scopes: q.scopes.clone(),
                seeds: vec![id],
                max_hops: 1,
                edge_types: None,
                direction: Direction::Both,
                as_of: q.as_of,
            })?;
            let mut common: Vec<NodeId> = cand_hop
                .nodes
                .iter()
                .map(|n| n.id)
                .filter(|nid| *nid != id && *nid != q.node && excluded.contains(nid))
                .collect();
            common.sort();
            out.push(LinkSuggestion {
                node: rec,
                score,
                common_neighbors: common,
                structural: in_structural.contains(&id),
                semantic: in_semantic.contains(&id),
            });
        }
        Ok(out)
    }
}
