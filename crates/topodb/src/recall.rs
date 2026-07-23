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
use crate::props::PropValue;
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
    /// legs run recency-free so the decay can't compound). `options.now_ms`
    /// pins BOTH clocks a call needs to be deterministic: the post-fusion
    /// recency decay's "now" (see above), AND the graph leg's `as_of`
    /// traversal time (`TraversalQuery::as_of`, passed through verbatim) — a
    /// test or replay that fixes `now_ms` gets a single consistent instant
    /// for every time-sensitive part of one `recall` call, not two that
    /// could drift apart.
    pub options: SearchOptions,
    /// Post-fusion label allowlist: when `Some`, only nodes carrying one of
    /// these labels survive fusion. `None` = unfiltered (today's behavior).
    /// `Some(vec![])` is `Rejected` — an empty allowlist admits nothing, so
    /// it is almost certainly a caller bug; omit the field to search
    /// unfiltered instead.
    pub labels: Option<Vec<String>>,
    /// Tombstone filter: when `Some(prop)`, a fused candidate is dropped if it
    /// carries that integer prop with a value `<=` the query's effective `now`
    /// (`options.now_ms`, else wall clock). Used to exclude memories a caller
    /// has marked superseded, while an `as_of`/`now_ms` set BEFORE the mark
    /// still sees them — the marker is a timestamp, not a delete. `None` =
    /// no tombstone filtering (today's behavior). Generic on purpose: the
    /// engine never names the prop.
    pub tombstone_prop: Option<String>,
    /// Text leg's RRF weight. Defaults to `WEIGHT_TEXT`.
    pub text_weight: f32,
    /// Vector leg's RRF weight. Defaults to `WEIGHT_VECTOR`.
    pub vector_weight: f32,
    /// Graph leg's RRF weight. Defaults to `WEIGHT_GRAPH`.
    pub graph_weight: f32,
    /// Opt-in post-fusion access boost (0.0-1.0, default 0 = off): each
    /// hit's fused score is multiplied by
    /// `1 + w·ln(1+count)/(1+ln(1+count))` using NON-BUMPING reads of the
    /// access counters — neutral for never-recalled nodes, log-damped,
    /// bounded below `1+w`. Live counters are db state: like wall-clock
    /// recency, results may shift as counters move.
    pub access_weight: f32,
    /// Post-fusion score multipliers by node label. Each `(label, weight)`
    /// applies a multiplicative factor to fused scores of matching nodes.
    /// Empty (the default) changes nothing. Each weight is validated
    /// finite and within 0.0..=10.0, same as the leg weights. Duplicate
    /// labels keep the first match. The engine is policy-free: this is
    /// mechanism only; policy defaults arrive from the host/MCP server.
    pub label_weights: Vec<(String, f32)>,
}

pub(crate) const WEIGHT_TEXT: f32 = 1.0;
pub(crate) const WEIGHT_VECTOR: f32 = 1.0;
/// Half weight for the graph leg: adjacency is corroboration, not
/// relevance — a 1-hop neighbor should never outrank a genuine text or
/// vector hit purely by being linked.
pub(crate) const WEIGHT_GRAPH: f32 = 0.5;
/// How many top preliminary-fusion nodes seed the graph leg's traversal —
/// bounded so `graph_boost` costs a handful of 1-hop reads, not one per
/// candidate in a potentially deep leg list.
pub(crate) const GRAPH_SEEDS: usize = 5;

impl RecallQuery {
    /// A query with every tunable at its default: stock leg weights, no
    /// label filter, no access boost, graph boost on, no vector leg, no
    /// expansions, default search options. Prefer struct-update over this
    /// (`RecallQuery { vector: …, ..RecallQuery::new(…) }`) so future
    /// tunables don't break your construction site.
    pub fn new(scopes: ScopeSet, query: impl Into<String>, k: usize) -> Self {
        Self {
            scopes,
            query: query.into(),
            k,
            vector: None,
            expansions: Vec::new(),
            graph_boost: true,
            options: SearchOptions::default(),
            labels: None,
            tombstone_prop: None,
            text_weight: WEIGHT_TEXT,
            vector_weight: WEIGHT_VECTOR,
            graph_weight: WEIGHT_GRAPH,
            access_weight: 0.0,
            label_weights: Vec::new(),
        }
    }

    /// Validates the tunable fields (weights, access_weight, labels shape).
    /// `Rejected` on any violation — checked by `recall` before any leg
    /// runs. Validation is over the WEIGHTS, not the effective legs: a
    /// caller can zero every leg that actually runs and legitimately get
    /// an empty result (degenerate-but-honest; see the design spec).
    pub(crate) fn validate_tuning(&self) -> Result<(), TopoError> {
        let weight_ok = |w: f32| w.is_finite() && (0.0..=10.0).contains(&w);
        for (name, w) in [
            ("text_weight", self.text_weight),
            ("vector_weight", self.vector_weight),
            ("graph_weight", self.graph_weight),
        ] {
            if !weight_ok(w) {
                return Err(TopoError::Rejected(format!(
                    "{name} must be finite and within 0.0..=10.0, got {w}"
                )));
            }
        }
        if self.text_weight == 0.0 && self.vector_weight == 0.0 && self.graph_weight == 0.0 {
            return Err(TopoError::Rejected(
                "at least one leg weight must be > 0.0".into(),
            ));
        }
        if !(self.access_weight.is_finite() && (0.0..=1.0).contains(&self.access_weight)) {
            return Err(TopoError::Rejected(format!(
                "access_weight must be finite and within 0.0..=1.0, got {}",
                self.access_weight
            )));
        }
        if let Some(labels) = &self.labels {
            if labels.is_empty() {
                return Err(TopoError::Rejected(
                    "labels must not be empty when present — an empty allowlist admits nothing; \
                     omit it to search unfiltered"
                        .into(),
                ));
            }
        }
        for (label, w) in &self.label_weights {
            if label.is_empty() {
                return Err(TopoError::Rejected(
                    "label_weights: label must not be empty".into(),
                ));
            }
            if !weight_ok(*w) {
                return Err(TopoError::Rejected(format!(
                    "label_weights[\"{}\"] must be finite and within 0.0..=10.0, got {w}",
                    label
                )));
            }
        }
        Ok(())
    }
}

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
        q.validate_tuning()?;
        // Check the CALLER's recency options before the leg call zeroes the
        // weight — see SearchOptions::validate_recency for why.
        q.options.validate_recency()?;
        if let Some((_, v)) = &q.vector {
            if v.is_empty() {
                return Err(TopoError::Rejected(
                    "recall query vector is empty (host must not send an empty embedding)".into(),
                ));
            }
        }
        let depth = leg_depth(q.k);

        // A zero-weight leg contributes nothing to a fused score but its ids
        // would still ride along at score 0.0 if the list were included
        // unconditionally (`rrf_fuse` does `entry().or_insert(0.0) += w /
        // rank`, which inserts an entry even at `w == 0.0`) — so "weight
        // says on, nothing actually ran" degenerates to Ok(empty)/ghost-free
        // rather than a ghost result. Applied symmetrically across all
        // three legs: skip the underlying read entirely when its weight is
        // 0.0, not just the fusion push — a leg that cannot contribute
        // shouldn't be paid for either.
        let mut records: std::collections::HashMap<crate::NodeId, NodeRecord> =
            std::collections::HashMap::new();
        let mut lists: Vec<(f32, Vec<crate::NodeId>)> = Vec::new();

        // Text leg runs recency-free: recency applies once, post-fusion.
        if q.text_weight > 0.0 {
            let mut leg_options = q.options.clone();
            leg_options.recency_weight = 0.0;
            let text_hits =
                self.search_text_expanded(&q.scopes, &q.query, depth, &leg_options, &q.expansions)?;
            let text_ids: Vec<crate::NodeId> = text_hits.iter().map(|(n, _)| n.id).collect();
            for (n, _) in text_hits {
                records.entry(n.id).or_insert(n);
            }
            lists.push((q.text_weight, text_ids));
        }

        // Vector leg: cosine over the scoped clusters for the named model.
        // An unknown model or a scope with no vectors is an EMPTY leg —
        // legitimately no data — never an error (contrast the empty-vector
        // rejection above, which is a host bug).
        if q.vector_weight > 0.0 {
            if let Some((model, vector)) = &q.vector {
                let vhits = self.search_vector(&crate::VectorQuery {
                    scopes: q.scopes.clone(),
                    model: model.clone(),
                    vector: vector.clone(),
                    k: depth,
                    candidates: None,
                })?;
                let vids: Vec<crate::NodeId> = vhits.iter().map(|(n, _)| n.id).collect();
                for (n, _) in vhits {
                    records.entry(n.id).or_insert(n);
                }
                lists.push((q.vector_weight, vids));
            }
        }
        // Graph leg (spec 2026-07-19): preliminary text+vector fusion picks
        // GRAPH_SEEDS seeds; ONE GRAPH_HOPS-bounded Both-direction traversal
        // (1 hop — eval-tuned, see ppr.rs) materializes their joint neighborhood;
        // PPR with teleport weighted by each
        // seed's preliminary score ranks it. Connectivity now orders the
        // list — a node multiple seeds converge on outranks a node dangling
        // off one seed — replacing the old flat seed-rank concatenation.
        // Seeds stay excluded from the list (ppr_over_subgraph's contract);
        // half weight as ever: adjacency is corroboration, not relevance.
        if q.graph_boost && q.graph_weight > 0.0 {
            let prelim = rrf_fuse(&lists);
            let seeds: Vec<(crate::NodeId, f32)> = prelim
                .iter()
                .take(GRAPH_SEEDS)
                .map(|(id, score)| (*id, *score))
                .collect();
            if !seeds.is_empty() {
                let sg = self.traverse(&crate::TraversalQuery {
                    scopes: q.scopes.clone(),
                    seeds: seeds.iter().map(|(id, _)| *id).collect(),
                    max_hops: crate::ppr::GRAPH_HOPS,
                    edge_types: None,
                    direction: crate::Direction::Both,
                    as_of: q.options.now_ms,
                })?;
                let scored = crate::ppr::ppr_over_subgraph(&sg, &seeds);
                let mut by_id: std::collections::HashMap<crate::NodeId, NodeRecord> =
                    sg.nodes.into_iter().map(|n| (n.id, n)).collect();
                let mut graph_ids: Vec<crate::NodeId> = Vec::new();
                for (id, _) in scored {
                    if let Some(rec) = by_id.remove(&id) {
                        graph_ids.push(id);
                        records.entry(id).or_insert(rec);
                    }
                }
                if !graph_ids.is_empty() {
                    lists.push((q.graph_weight, graph_ids));
                }
            }
        }
        let fused = rrf_fuse(&lists);

        let mut out: Vec<(NodeRecord, f32)> = fused
            .into_iter()
            .filter_map(|(id, score)| records.remove(&id).map(|n| (n, score)))
            .collect();
        // Post-fusion label allowlist (spec: filter BEFORE adjustments so
        // counter reads aren't spent on filtered nodes; leg depth >> k, so
        // filtering rarely starves k — and legitimately may).
        if let Some(labels) = &q.labels {
            out.retain(|(n, _)| labels.iter().any(|l| n.label == l.as_str()));
        }
        // Tombstone filter: drop candidates the caller marked superseded as of
        // this query's "now". The marker is a timestamp, so an as_of/now_ms set
        // BEFORE the mark keeps the node (its tombstone is in that query's
        // future) — supersession dates a fact, it doesn't erase its history.
        if let Some(prop) = &q.tombstone_prop {
            let now = q.options.now_ms.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            });
            out.retain(|(n, _)| match n.props.get(prop) {
                Some(PropValue::Int(ts)) => *ts > now,
                _ => true,
            });
        }
        apply_adjustments(
            &mut out,
            &q.options,
            q.access_weight,
            &q.label_weights,
            &|id| self.access_count_unbumped(id),
        );
        out.truncate(q.k);
        Ok(out)
    }
}

/// Post-fusion access factor: neutral at count 0, log-damped (recall's own
/// reads bump the counters this reads — damping keeps the loop from
/// running away), bounded below `1 + weight`. See the design spec.
pub(crate) fn access_factor(weight: f32, count: u64) -> f32 {
    if weight <= 0.0 || count == 0 {
        return 1.0;
    }
    let l = ((count as f32) + 1.0).ln();
    1.0 + weight * l / (1.0 + l)
}

/// Combined post-fusion adjustment: recency (the same
/// `(1-w) + w·2^(-age/half_life)` factor `search_text_with` uses), the
/// opt-in access boost (`access_factor`), and label-based score multipliers
/// multiply into one factor per candidate, applied once to fused scores, then
/// re-sorted (score desc, id asc). No-op — and no counter reads — when
/// recency weight is 0, access_weight is 0, AND label_weights is empty,
/// preserving today's early-return shape and the byte-identical-defaults
/// requirement.
pub(crate) fn apply_adjustments(
    out: &mut [(NodeRecord, f32)],
    options: &SearchOptions,
    access_weight: f32,
    label_weights: &[(String, f32)],
    counts: &dyn Fn(crate::NodeId) -> u64,
) {
    let w = options.recency_weight;
    if w <= 0.0 && access_weight <= 0.0 && label_weights.is_empty() {
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
        let mut factor = 1.0f32;
        if w > 0.0 {
            let age = (now - rec.id.timestamp_ms() as i64).max(0) as f32;
            factor *= (1.0 - w) + w * (-(age / half_life)).exp2();
        }
        factor *= access_factor(
            access_weight,
            if access_weight > 0.0 {
                counts(rec.id)
            } else {
                0
            },
        );
        // Apply label-based score multiplier: first match wins, duplicate
        // labels keep the first.
        if let Some((_, wl)) = label_weights.iter().find(|(l, _)| rec.label == l.as_str()) {
            factor *= wl;
        }
        *score *= factor;
    }
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
}

#[cfg(test)]
mod adjustment_tests {
    use super::*;
    use crate::ids::Scope;
    use smol_str::SmolStr;

    fn node_with_label(label: &str, id_num: u128) -> NodeRecord {
        NodeRecord {
            id: crate::NodeId::from_u128(id_num),
            scope: Scope::Id(crate::ids::ScopeId::new()),
            label: SmolStr::new(label),
            props: std::collections::BTreeMap::new(),
            embedding: None,
        }
    }

    fn apply_adjustments_with_labels(
        out: &mut [(NodeRecord, f32)],
        label_weights: &[(String, f32)],
    ) {
        apply_adjustments(out, &SearchOptions::default(), 0.0, label_weights, &|_| 0);
    }

    #[test]
    fn access_factor_is_neutral_monotone_bounded() {
        let f = |w: f32, count: u64| access_factor(w, count);
        assert_eq!(f(1.0, 0), 1.0, "neutral at zero count");
        assert_eq!(f(0.0, 1_000_000), 1.0, "neutral at zero weight");
        assert!(f(1.0, 1) > f(1.0, 0));
        assert!(f(1.0, 100) > f(1.0, 10));
        assert!(f(1.0, u64::MAX) < 2.0, "bounded below 1 + weight");
        assert!(f(0.5, u64::MAX) < 1.5);
    }

    #[test]
    fn label_weight_downranks_matching_label() {
        // Two candidates with adjacent fused scores; the down-weighted label's
        // node must drop below the other after adjustment.
        let mut out = vec![
            (node_with_label("Entity", 1), 0.020f32),
            (node_with_label("Memory", 2), 0.019f32),
        ];
        apply_adjustments_with_labels(&mut out, &[("Entity".to_string(), 0.5)]);
        assert_eq!(
            out[0].0.label.to_string(),
            "Memory",
            "down-weighted Entity must drop"
        );
        assert!(
            (out[1].1 - 0.010).abs() < 1e-6,
            "factor applied multiplicatively"
        );
    }

    #[test]
    fn empty_label_weights_change_nothing() {
        // Same input twice: empty label_weights must be byte-identical to the
        // pre-change behavior (scores AND order).
        let mut out = vec![
            (node_with_label("Entity", 1), 0.020f32),
            (node_with_label("Memory", 2), 0.019f32),
        ];
        let original_order = out.iter().map(|(n, s)| (n.id, *s)).collect::<Vec<_>>();
        let original_scores = out.iter().map(|(_, s)| *s).collect::<Vec<_>>();

        apply_adjustments_with_labels(&mut out, &[]);

        let new_order = out.iter().map(|(n, s)| (n.id, *s)).collect::<Vec<_>>();
        let new_scores = out.iter().map(|(_, s)| *s).collect::<Vec<_>>();

        assert_eq!(
            original_order, new_order,
            "order must not change with empty label_weights"
        );
        assert_eq!(
            original_scores, new_scores,
            "scores must not change with empty label_weights"
        );
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;
    use crate::ids::ScopeSet;

    fn q() -> RecallQuery {
        RecallQuery::new(ScopeSet::of(&[crate::ids::ScopeId::new()]), "hello", 5)
    }

    #[test]
    fn new_carries_todays_defaults() {
        let q = q();
        assert_eq!(q.text_weight, WEIGHT_TEXT);
        assert_eq!(q.vector_weight, WEIGHT_VECTOR);
        assert_eq!(q.graph_weight, WEIGHT_GRAPH);
        assert_eq!(q.access_weight, 0.0);
        assert!(q.labels.is_none());
        assert!(q.graph_boost);
        assert!(q.vector.is_none());
    }

    #[test]
    fn weight_validation_rejects_bad_values() {
        for build in [
            |mut x: RecallQuery| {
                x.text_weight = -0.1;
                x
            },
            |mut x: RecallQuery| {
                x.vector_weight = f32::NAN;
                x
            },
            |mut x: RecallQuery| {
                x.graph_weight = 10.1;
                x
            },
            |mut x: RecallQuery| {
                x.access_weight = 1.1;
                x
            },
            |mut x: RecallQuery| {
                x.access_weight = f32::INFINITY;
                x
            },
            |mut x: RecallQuery| {
                x.text_weight = 0.0;
                x.vector_weight = 0.0;
                x.graph_weight = 0.0;
                x
            },
            |mut x: RecallQuery| {
                x.labels = Some(vec![]);
                x
            },
        ] {
            let bad = build(q());
            assert!(
                matches!(bad.validate_tuning(), Err(crate::TopoError::Rejected(_))),
                "must reject: {bad:?}"
            );
        }
    }

    #[test]
    fn label_weights_validation_rejects_bad_factors() {
        // NaN, negative, and 10.1 all rejected; 0.0 and 10.0 accepted.
        for build in [
            |mut x: RecallQuery| {
                x.label_weights = vec![("Entity".to_string(), -0.1)];
                x
            },
            |mut x: RecallQuery| {
                x.label_weights = vec![("Entity".to_string(), f32::NAN)];
                x
            },
            |mut x: RecallQuery| {
                x.label_weights = vec![("Entity".to_string(), 10.1)];
                x
            },
            |mut x: RecallQuery| {
                x.label_weights = vec![("".to_string(), 0.5)];
                x
            },
        ] {
            let bad = build(q());
            assert!(
                matches!(bad.validate_tuning(), Err(crate::TopoError::Rejected(_))),
                "must reject: {bad:?}"
            );
        }

        // Valid edge cases should pass.
        assert!(RecallQuery {
            label_weights: vec![("Entity".to_string(), 0.0)],
            ..q()
        }
        .validate_tuning()
        .is_ok());
        assert!(RecallQuery {
            label_weights: vec![("Entity".to_string(), 10.0)],
            ..q()
        }
        .validate_tuning()
        .is_ok());
        assert!(RecallQuery {
            label_weights: vec![("Entity".to_string(), 0.5), ("Memory".to_string(), 1.5)],
            ..q()
        }
        .validate_tuning()
        .is_ok());
    }

    #[test]
    fn default_tuning_validates() {
        assert!(q().validate_tuning().is_ok());
    }
}
