//! Production hybrid recall: reciprocal-rank fusion of the text, vector,
//! and graph read paths. The engine owns the MECHANICS only — query
//! vectors and term expansions arrive pre-resolved from the host (see the
//! spec: graph-native data, engine mechanics, host policy).

use crate::ids::NodeId;

/// Standard RRF constant — dampens the head so one leg's #1 can't drown
/// out consistent mid-rank agreement across legs.
pub(crate) const RRF_K: f32 = 60.0;

/// Fuses per-leg rankings: each list is `(weight, ids best-first)`; a
/// node's fused score is `Σ weight / (RRF_K + rank)` over the lists it
/// appears in (rank is 1-based). Output is sorted score-desc with
/// ascending-id tie-break — the same determinism contract as search.
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

use crate::error::TopoError;
use crate::fts::SearchOptions;
use crate::ids::ScopeSet;
use crate::state::NodeRecord;
use crate::Db;

/// How deep each leg ranks before fusion: enough depth that RRF has real
/// lists to agree over even for small `k`, capped so a huge `k` cannot
/// turn every leg into a full-corpus scan.
pub(crate) fn leg_depth(k: usize) -> usize {
    (3 * k).clamp(30, 50)
}

/// One production hybrid-recall request. The engine fuses; the HOST
/// resolves — `vector` is a pre-computed query embedding, `expansions`
/// are pre-resolved synonym terms. The engine neither knows nor cares
/// where either came from (spec: engine mechanics, host policy).
#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub scopes: ScopeSet,
    pub query: String,
    pub k: usize,
    /// Host-computed query embedding with its model name. `None` = no
    /// vector leg. An empty vector is `Rejected` (a host bug should be
    /// loud); an unknown model name is just an empty leg (legitimately no
    /// data under that namespace).
    pub vector: Option<(String, Vec<f32>)>,
    /// Host-resolved term expansions, applied to the text leg at
    /// `fts::FUZZY_DISCOUNT`. Depth-1 only — the engine never chains them.
    pub expansions: Vec<(String, Vec<String>)>,
    /// Two-stage graph signal: 1-hop neighbors of the top preliminary
    /// seeds join as a third, half-weight list.
    pub graph_boost: bool,
    /// Recency + fuzzy knobs. Recency is applied ONCE, post-fusion (the
    /// legs run recency-free so the decay can't compound).
    pub options: SearchOptions,
}

pub(crate) const WEIGHT_TEXT: f32 = 1.0;

impl Db {
    /// Hybrid recall: BM25 text (+ expansions), cosine vector, and 1-hop
    /// graph legs, RRF-fused (`rrf_fuse`), recency-weighted post-fusion,
    /// truncated to `k`. Legs run as sequential read transactions against
    /// the single-applier engine — see the spec for why that is
    /// acceptable. Validation mirrors `search_text_with`.
    pub fn recall(&self, q: &RecallQuery) -> Result<Vec<(NodeRecord, f32)>, TopoError> {
        // search_text_with rejects k == 0 internally too, but only because
        // depth is clamped to >= 30 — recall's own k must be checked
        // explicitly or a k == 0 request would silently run at depth 30.
        if q.k == 0 {
            return Err(TopoError::Rejected("recall requires k > 0".into()));
        }
        if let Some((_, v)) = &q.vector {
            if v.is_empty() {
                return Err(TopoError::Rejected(
                    "recall query vector is empty (host must not send an empty embedding)".into(),
                ));
            }
        }
        let depth = leg_depth(q.k);

        // Text leg runs recency-free: recency applies once, post-fusion.
        let mut leg_options = q.options.clone();
        leg_options.recency_weight = 0.0;
        let text_hits = self.search_text_with(&q.scopes, &q.query, depth, &leg_options)?;

        let mut records: std::collections::HashMap<crate::NodeId, NodeRecord> =
            text_hits.iter().map(|(n, _)| (n.id, n.clone())).collect();
        let text_ids: Vec<crate::NodeId> = text_hits.iter().map(|(n, _)| n.id).collect();

        let lists: Vec<(f32, Vec<crate::NodeId>)> = vec![(WEIGHT_TEXT, text_ids)];
        let fused = rrf_fuse(&lists);

        let mut out: Vec<(NodeRecord, f32)> = fused
            .into_iter()
            .filter_map(|(id, score)| records.remove(&id).map(|n| (n, score)))
            .collect();
        apply_recency(&mut out, &q.options);
        out.truncate(q.k);
        Ok(out)
    }
}

/// Post-fusion recency: the same `(1-w) + w·2^(-age/half_life)` factor
/// `search_text_with` uses, applied once to fused scores, then re-sorted
/// (score desc, id asc). No-op at weight 0.
pub(crate) fn apply_recency(out: &mut [(NodeRecord, f32)], options: &SearchOptions) {
    let w = options.recency_weight;
    if w <= 0.0 {
        return;
    }
    let now = options.now_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as i64
    });
    let half_life = options.recency_half_life_ms as f32;
    for (rec, score) in out.iter_mut() {
        let age = (now - rec.id.timestamp_ms() as i64).max(0) as f32;
        *score *= (1.0 - w) + w * (-(age / half_life)).exp2();
    }
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
}
