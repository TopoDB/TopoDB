//! Production hybrid recall: reciprocal-rank fusion of the text, vector,
//! and graph read paths. The engine owns the MECHANICS only — query
//! vectors and term expansions arrive pre-resolved from the host (see the
//! spec: graph-native data, engine mechanics, host policy).

use crate::ids::NodeId;

/// Standard RRF constant — dampens the head so one leg's #1 can't drown
/// out consistent mid-rank agreement across legs.
#[allow(dead_code)]
pub(crate) const RRF_K: f32 = 60.0;

/// Fuses per-leg rankings: each list is `(weight, ids best-first)`; a
/// node's fused score is `Σ weight / (RRF_K + rank)` over the lists it
/// appears in (rank is 1-based). Output is sorted score-desc with
/// ascending-id tie-break — the same determinism contract as search.
#[allow(dead_code)]
pub(crate) fn rrf_fuse(lists: &[(f32, Vec<NodeId>)]) -> Vec<(NodeId, f32)> {
    use std::collections::HashMap;
    let mut scores: HashMap<NodeId, f32> = HashMap::new();
    for (weight, ids) in lists {
        for (i, id) in ids.iter().enumerate() {
            *scores.entry(*id).or_insert(0.0) += weight / (RRF_K + (i as f32 + 1.0));
        }
    }
    let mut out: Vec<(NodeId, f32)> = scores.into_iter().collect();
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

    fn id(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    #[test]
    fn agreement_across_legs_beats_single_leg_top_rank() {
        // B is #2 in both legs; A and C are #1 in one leg each.
        // 2/(60+2) = 0.0323 > 1/(60+1) = 0.0164 — agreement wins.
        let fused = rrf_fuse(&[
            (1.0, vec![id(1), id(2)]), // text: A, B
            (1.0, vec![id(3), id(2)]), // vector: C, B
        ]);
        assert_eq!(fused[0].0, id(2), "B (agreement) must rank first");
    }

    #[test]
    fn weights_scale_leg_contributions() {
        let fused = rrf_fuse(&[(1.0, vec![id(1)]), (0.5, vec![id(2)])]);
        assert_eq!(fused[0].0, id(1));
        assert!((fused[0].1 - 1.0 / 61.0).abs() < 1e-6);
        assert!((fused[1].1 - 0.5 / 61.0).abs() < 1e-6);
    }

    #[test]
    fn ties_break_by_ascending_id() {
        let fused = rrf_fuse(&[(1.0, vec![id(9)]), (1.0, vec![id(3)])]);
        assert_eq!(fused[0].0, id(3), "equal scores: lower id first");
    }

    #[test]
    fn empty_input_fuses_to_empty() {
        assert!(rrf_fuse(&[]).is_empty());
        assert!(rrf_fuse(&[(1.0, vec![])]).is_empty());
    }
}
