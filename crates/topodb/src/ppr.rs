//! Personalized PageRank over a `traverse`-materialized subgraph.
//! Pure mechanics: no storage access, no RNG, no wall clock — byte-identical
//! output for identical input. See the 2026-07-19 PPR design spec.

use crate::ids::NodeId;
use crate::read::Subgraph;
use std::collections::{HashMap, HashSet};

/// Damping: probability of following an edge vs. teleporting to a seed.
pub(crate) const ALPHA: f64 = 0.85;
/// Power-iteration cap; convergence usually exits far earlier.
pub(crate) const PPR_MAX_ITERS: usize = 30;
/// L1-delta early-exit threshold.
pub(crate) const PPR_EPSILON: f64 = 1e-9;
/// Recall graph-leg traversal bound. Tuned DOWN from the design's
/// initial 2 via the golden-set eval (the constants' stated tuning
/// mechanism): at 2 hops, hub-adjacent nodes reached through entity fan-
/// out picked up enough graph-leg RRF mass to push a correct-but-unlinked
/// top text/vector hit out of the eval's top-3 ("search index rebuilding
/// tripling deploy time"). At 1 hop the membership matches the old flat
/// leg — PPR improves the ORDERING only — and the full eval stays green.
///
/// Note: `traverse` collects edges only while expanding nodes within the
/// hop budget, so at 1 hop the subgraph is the union of seed stars —
/// neighbor↔neighbor edges are absent and PPR ranks by weighted
/// seed-adjacency.
pub(crate) const GRAPH_HOPS: u8 = 1;
/// `suggest_links` traversal bound.
pub(crate) const SUGGEST_HOPS: u8 = 3;
/// `suggest_links` RRF weights: structural (PPR) / semantic (cosine) legs.
pub(crate) const WEIGHT_STRUCTURAL: f32 = 1.0;
pub(crate) const WEIGHT_SEMANTIC: f32 = 1.0;

/// PPR over `sg` treated as undirected; parallel edges between a pair
/// collapse to a single weight-1 link. `seeds` carry teleport weights
/// (normalized internally; non-positive weights and seeds absent from the
/// subgraph are dropped). Returns every NON-seed node scored, sorted
/// score-desc with ascending-id tie-break. Empty when the subgraph or the
/// effective seed set is empty.
pub(crate) fn ppr_over_subgraph(sg: &Subgraph, seeds: &[(NodeId, f32)]) -> Vec<(NodeId, f32)> {
    // Ascending-id index: the single source of iteration order everywhere
    // below — this, not luck, is what makes the function deterministic.
    let mut ids: Vec<NodeId> = sg.nodes.iter().map(|n| n.id).collect();
    ids.sort();
    ids.dedup();
    let n = ids.len();
    if n == 0 {
        return Vec::new();
    }
    let index: HashMap<NodeId, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // Undirected adjacency; a parallel-edge pair collapses to one link.
    let mut pairs: Vec<(usize, usize)> = sg
        .edges
        .iter()
        .filter_map(|e| Some((*index.get(&e.from)?, *index.get(&e.to)?)))
        .filter(|(a, b)| a != b)
        .map(|(a, b)| (a.min(b), a.max(b)))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (a, b) in pairs {
        adj[a].push(b);
        adj[b].push(a);
    }

    // Teleport vector: named seeds present in the subgraph, positive
    // weights only, normalized to sum 1. Every NAMED-and-present seed is
    // excluded from the output regardless of weight.
    let mut teleport = vec![0.0f64; n];
    let mut seed_ix: HashSet<usize> = HashSet::new();
    let mut total = 0.0f64;
    for (id, w) in seeds {
        if let Some(&i) = index.get(id) {
            seed_ix.insert(i);
            if *w > 0.0 && w.is_finite() {
                teleport[i] += f64::from(*w);
                total += f64::from(*w);
            }
        }
    }
    if total <= 0.0 {
        return Vec::new();
    }
    for t in &mut teleport {
        *t /= total;
    }

    // Jacobi power iteration: p ← (1−α)·s + α·(Wᵀp + dangling_mass·s).
    // Dangling (edgeless) nodes hand their mass back to the seeds — no leak.
    let mut p = teleport.clone();
    for _ in 0..PPR_MAX_ITERS {
        let mut pushed = vec![0.0f64; n];
        let mut dangling = 0.0f64;
        for i in 0..n {
            if adj[i].is_empty() {
                dangling += p[i];
                continue;
            }
            let share = p[i] / adj[i].len() as f64;
            for &j in &adj[i] {
                pushed[j] += share;
            }
        }
        let mut delta = 0.0f64;
        for i in 0..n {
            let next = (1.0 - ALPHA) * teleport[i] + ALPHA * (pushed[i] + dangling * teleport[i]);
            delta += (next - p[i]).abs();
            p[i] = next;
        }
        if delta < PPR_EPSILON {
            break;
        }
    }

    let mut out: Vec<(NodeId, f32)> = ids
        .iter()
        .enumerate()
        .filter(|(i, _)| !seed_ix.contains(i))
        .map(|(i, &id)| (id, p[i] as f32))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{EdgeId, Scope};
    use crate::state::{EdgeRecord, NodeRecord};

    fn node(id: NodeId) -> NodeRecord {
        NodeRecord {
            id,
            scope: Scope::Shared,
            label: "T".into(),
            props: Default::default(),
            embedding: None,
        }
    }

    fn edge(from: NodeId, to: NodeId) -> EdgeRecord {
        EdgeRecord {
            id: EdgeId::new(),
            scope: Scope::Shared,
            ty: "R".into(),
            from,
            to,
            props: Default::default(),
            valid_from: 0,
            valid_to: None,
        }
    }

    /// n ids with GUARANTEED ascending order. Never use `NodeId::new()`
    /// where relative order matters — `Ulid::new()` is not monotonic
    /// within a millisecond; `from_u128` is the codebase's deterministic
    /// fixture constructor (see `ids.rs`).
    fn ids(n: usize) -> Vec<NodeId> {
        (1..=n).map(|i| NodeId::from_u128(i as u128)).collect()
    }

    fn sg(nodes: &[NodeId], edges: &[(NodeId, NodeId)]) -> Subgraph {
        Subgraph {
            nodes: nodes.iter().map(|&i| node(i)).collect(),
            edges: edges.iter().map(|&(f, t)| edge(f, t)).collect(),
        }
    }

    #[test]
    fn multi_seed_convergence_beats_single_link() {
        // x linked to all three seeds; y linked to one. x must outrank y.
        let v = ids(5);
        let (s1, s2, s3, y, x) = (v[0], v[1], v[2], v[3], v[4]);
        let g = sg(&v, &[(s1, x), (s2, x), (s3, x), (s1, y)]);
        let scored = ppr_over_subgraph(&g, &[(s1, 1.0), (s2, 1.0), (s3, 1.0)]);
        assert_eq!(scored[0].0, x, "3-seed neighbor must rank first");
        assert!(scored[0].1 > scored[1].1, "and strictly outscore y");
    }

    #[test]
    fn chain_scores_decay_with_distance() {
        let v = ids(4);
        let g = sg(&v, &[(v[0], v[1]), (v[1], v[2]), (v[2], v[3])]);
        let scored = ppr_over_subgraph(&g, &[(v[0], 1.0)]);
        let score = |id: NodeId| scored.iter().find(|(n, _)| *n == id).unwrap().1;
        assert!(score(v[1]) > score(v[2]), "1 hop must beat 2 hops");
        assert!(score(v[2]) > score(v[3]), "2 hops must beat 3 hops");
    }

    #[test]
    fn seeds_are_excluded_from_output() {
        let v = ids(3);
        let g = sg(&v, &[(v[0], v[1]), (v[1], v[2])]);
        let scored = ppr_over_subgraph(&g, &[(v[0], 1.0)]);
        assert!(scored.iter().all(|(id, _)| *id != v[0]));
        assert_eq!(scored.len(), 2);
    }

    #[test]
    fn equal_scores_tie_break_by_ascending_id() {
        // Symmetric star: two leaves off one seed are exactly tied.
        let v = ids(3);
        let g = sg(&v, &[(v[0], v[1]), (v[0], v[2])]);
        let scored = ppr_over_subgraph(&g, &[(v[0], 1.0)]);
        assert_eq!(scored[0].0, v[1], "lower id first on tie");
        assert_eq!(scored[1].0, v[2]);
        assert!((scored[0].1 - scored[1].1).abs() < 1e-12);
    }

    #[test]
    fn weighted_teleport_biases_toward_heavy_seed() {
        let v = ids(4);
        let (s_heavy, s_light, x, y) = (v[0], v[1], v[2], v[3]);
        let g = sg(&v, &[(s_heavy, x), (s_light, y)]);
        let scored = ppr_over_subgraph(&g, &[(s_heavy, 0.9), (s_light, 0.1)]);
        assert_eq!(scored[0].0, x, "heavy seed's neighbor must rank first");
    }

    #[test]
    fn parallel_edges_collapse_to_one_link() {
        // y triple-linked to the seed, x single-linked: must TIE, not win.
        let v = ids(3);
        let (s, x, y) = (v[0], v[1], v[2]);
        let g = sg(&v, &[(s, x), (s, y), (s, y), (y, s)]);
        let scored = ppr_over_subgraph(&g, &[(s, 1.0)]);
        assert!(
            (scored[0].1 - scored[1].1).abs() < 1e-12,
            "collapsed parallel edges must not distort the walk"
        );
        assert_eq!(scored[0].0, x, "tie broken by ascending id");
    }

    #[test]
    fn isolated_node_scores_zero_and_nothing_nans() {
        let v = ids(3);
        let g = sg(&v, &[(v[0], v[1])]); // v[2] has no edges
        let scored = ppr_over_subgraph(&g, &[(v[0], 1.0)]);
        let iso = scored.iter().find(|(n, _)| *n == v[2]).unwrap();
        assert!(iso.1.is_finite());
        assert!(iso.1 < 1e-9, "unreachable node gets (near-)zero mass");
        // Mass conservation: non-seed mass can never exceed the total.
        let total: f64 = scored.iter().map(|(_, s)| f64::from(*s)).sum();
        assert!(total <= 1.0 + 1e-9, "mass must not be created, got {total}");
    }

    #[test]
    fn absent_or_nonpositive_seeds_are_dropped() {
        let v = ids(2);
        let g = sg(&v, &[(v[0], v[1])]);
        let ghost = NodeId::from_u128(999); // not in the subgraph
        let scored = ppr_over_subgraph(&g, &[(ghost, 1.0), (v[0], 1.0), (v[1], -3.0)]);
        // ghost dropped; v[1]'s non-positive weight dropped (it stays a
        // seed for EXCLUSION purposes — it was named — but adds no mass).
        assert!(scored.is_empty() || scored.iter().all(|(id, _)| *id != v[0]));
        // All seeds absent → empty.
        assert!(ppr_over_subgraph(&g, &[(ghost, 1.0)]).is_empty());
        // Empty subgraph → empty.
        assert!(ppr_over_subgraph(&sg(&[], &[]), &[(v[0], 1.0)]).is_empty());
    }

    #[test]
    fn byte_identical_across_runs() {
        let v = ids(6);
        let g = sg(
            &v,
            &[
                (v[0], v[2]),
                (v[1], v[2]),
                (v[2], v[3]),
                (v[3], v[4]),
                (v[1], v[5]),
            ],
        );
        let seeds = [(v[0], 0.7), (v[1], 0.3)];
        assert_eq!(
            ppr_over_subgraph(&g, &seeds),
            ppr_over_subgraph(&g, &seeds),
            "identical input must give identical output, bit for bit"
        );
    }
}
