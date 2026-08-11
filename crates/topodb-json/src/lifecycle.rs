//! Phase C of the memory lifecycle: the decay-candidate sweep. Deterministic,
//! unbumped, read-only — it surfaces evidence and NEVER stamps anything (the
//! judge acts through `forget`/`consolidate_memories`, spec decision 5). All
//! policy lives here: the engine only supplies the non-bumping scan
//! primitives (`nodes_by_label_unbumped`, `access_stats`).

use crate::{
    ComposeError, MEMORY_CONTENT_PROP, MEMORY_KINDS, MEMORY_KIND_DECISION, MEMORY_KIND_DEFAULT,
    MEMORY_KIND_EPISODIC, MEMORY_KIND_PROCEDURAL, MEMORY_KIND_PROP, MEMORY_LABEL,
    MEMORY_TOMBSTONE_PROPS,
};
use topodb::{Db, NodeId, Op, PropValue, ScopeSet};

/// How many candidates a sweep reports by default.
pub const LIFECYCLE_DEFAULT_LIMIT: usize = 20;
/// Per-kind staleness half-lives (spec-fixed defaults, tunable per call):
/// an episodic observation goes stale in weeks, a standing fact in months,
/// a how-to in a year.
pub const LIFECYCLE_HALF_LIFE_EPISODIC_DAYS: f64 = 14.0;
pub const LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS: f64 = 120.0;
pub const LIFECYCLE_HALF_LIFE_PROCEDURAL_DAYS: f64 = 365.0;
/// A deliberate tie to the semantic constant: decisions age like standing
/// facts. Change independently if dogfood shows otherwise.
pub const LIFECYCLE_HALF_LIFE_DECISION_DAYS: f64 = LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS;

const DAY_MS: f64 = 86_400_000.0;

/// Tunables for one sweep. `Default` is the spec's policy.
#[derive(Debug, Clone)]
pub struct LifecycleParams {
    /// Top-N candidates to report (by descending staleness).
    pub limit: usize,
    pub half_life_episodic_ms: i64,
    pub half_life_semantic_ms: i64,
    pub half_life_procedural_ms: i64,
    pub half_life_decision_ms: i64,
}

impl Default for LifecycleParams {
    fn default() -> Self {
        Self {
            limit: LIFECYCLE_DEFAULT_LIMIT,
            half_life_episodic_ms: (LIFECYCLE_HALF_LIFE_EPISODIC_DAYS * DAY_MS) as i64,
            half_life_semantic_ms: (LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS * DAY_MS) as i64,
            half_life_procedural_ms: (LIFECYCLE_HALF_LIFE_PROCEDURAL_DAYS * DAY_MS) as i64,
            half_life_decision_ms: (LIFECYCLE_HALF_LIFE_DECISION_DAYS * DAY_MS) as i64,
        }
    }
}

/// One decay candidate with its full evidence — everything the judge needs
/// to decide keep|forget without another lookup.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LifecycleCandidate {
    pub id: String,
    pub content: String,
    /// The EFFECTIVE kind: the stored value when valid, else `semantic`
    /// (absent means semantic; an out-of-vocabulary value degrades to the
    /// default rather than erroring a read).
    pub kind: String,
    /// The node id's ULID mint timestamp (ms).
    pub created_at: i64,
    /// Raw counter value; `0` = never accessed (the age computation falls
    /// back to `created_at`, this field stays raw evidence).
    pub last_accessed_at: i64,
    pub access_count: u64,
    pub staleness: f64,
}

/// The spec's staleness score: `(age / half_life) / ln(e + access_count)`.
/// Negative ages (a node minted after `now` — clock skew or a backdated
/// sweep) clamp to 0. Pure and exported so tests and future policy layers
/// can pin the formula itself.
pub fn staleness(age_ms: i64, half_life_ms: i64, access_count: u64) -> f64 {
    let age = age_ms.max(0) as f64;
    (age / half_life_ms as f64) / (std::f64::consts::E + access_count as f64).ln()
}

/// The Phase C sweep: rank live memories in `scopes` by staleness and
/// return the top `params.limit` with full evidence. Read-only and
/// unbumped end to end. `now_ms` is injectable for determinism.
pub fn lifecycle_candidates(
    db: &Db,
    scopes: &ScopeSet,
    params: &LifecycleParams,
    now_ms: i64,
) -> Result<Vec<LifecycleCandidate>, ComposeError> {
    if params.limit == 0 {
        return Err(ComposeError::Invalid("limit must be at least 1".into()));
    }
    for (kind, hl) in [
        (MEMORY_KIND_EPISODIC, params.half_life_episodic_ms),
        (crate::MEMORY_KIND_SEMANTIC, params.half_life_semantic_ms),
        (MEMORY_KIND_PROCEDURAL, params.half_life_procedural_ms),
        (MEMORY_KIND_DECISION, params.half_life_decision_ms),
    ] {
        if hl <= 0 {
            return Err(ComposeError::Invalid(format!(
                "half-lives must be positive, got {hl}ms for {kind}"
            )));
        }
    }

    let mut out: Vec<LifecycleCandidate> = Vec::new();
    for node in db.nodes_by_label_unbumped(scopes, MEMORY_LABEL) {
        // Same liveness motion as every hygiene scan: a tombstone key's
        // presence retires the memory from proposal.
        if MEMORY_TOMBSTONE_PROPS
            .iter()
            .any(|p| node.props.contains_key(*p))
        {
            continue;
        }
        let content = match node.props.get(MEMORY_CONTENT_PROP) {
            Some(PropValue::Str(c)) => c.clone(),
            _ => continue,
        };
        let kind = match node.props.get(MEMORY_KIND_PROP) {
            Some(PropValue::Str(k)) if MEMORY_KINDS.contains(&k.as_str()) => k.clone(),
            _ => MEMORY_KIND_DEFAULT.to_string(),
        };
        let half_life_ms = match kind.as_str() {
            MEMORY_KIND_EPISODIC => params.half_life_episodic_ms,
            MEMORY_KIND_PROCEDURAL => params.half_life_procedural_ms,
            // Named explicitly, not left to the fall-through: the shared
            // semantic tunable IS decision's half-life (deliberate 120d tie).
            MEMORY_KIND_DECISION => params.half_life_decision_ms,
            _ => params.half_life_semantic_ms,
        };
        let stats = db.access_stats(scopes, node.id)?.unwrap_or_default();
        let created_at = node.id.timestamp_ms() as i64;
        let age_ms = now_ms - stats.last_accessed_at.max(created_at);
        out.push(LifecycleCandidate {
            id: node.id.to_string(),
            content,
            kind,
            created_at,
            last_accessed_at: stats.last_accessed_at,
            access_count: stats.access_count,
            staleness: staleness(age_ms, half_life_ms, stats.access_count),
        });
    }

    // Descending staleness; ties by ascending id — fully deterministic
    // under an injected now_ms. staleness is never NaN (half-lives are
    // validated positive, ages clamp at 0), so partial_cmp cannot fail.
    out.sort_by(|a, b| {
        b.staleness
            .partial_cmp(&a.staleness)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(params.limit);
    Ok(out)
}

/// Phase E: plan the destructive purge. Selects every Memory node in
/// `scopes` whose ANY tombstone prop holds an `Int` strictly older than
/// `tombstoned_before_ms` and returns one `Op::RemoveNode` per hit plus
/// the ascending-sorted id list (same order). The caller decides whether
/// to submit — the CLI's dry-run prints this plan without ever writing.
/// Non-`Int` tombstone values are not marks (the engine's liveness rule);
/// live nodes are never selected. Purge is deliberately CLI-only and
/// never part of the lifecycle graph: reclamation is an operator action.
pub fn plan_purge(
    db: &Db,
    scopes: &ScopeSet,
    tombstoned_before_ms: i64,
) -> Result<(Vec<Op>, Vec<String>), ComposeError> {
    if tombstoned_before_ms <= 0 {
        return Err(ComposeError::Invalid(
            "tombstoned-before must be a positive unix-ms timestamp".into(),
        ));
    }
    let mut doomed: Vec<NodeId> = Vec::new();
    for node in db.nodes_by_label_unbumped(scopes, MEMORY_LABEL) {
        let qualifies = MEMORY_TOMBSTONE_PROPS.iter().any(
            |p| matches!(node.props.get(*p), Some(PropValue::Int(t)) if *t < tombstoned_before_ms),
        );
        if qualifies {
            doomed.push(node.id);
        }
    }
    doomed.sort();
    let ids = doomed.iter().map(|id| id.to_string()).collect();
    let ops = doomed.into_iter().map(|id| Op::RemoveNode { id }).collect();
    Ok((ops, ids))
}

/// The kind→half-life map for SEARCH RANKING, built from the same
/// constants the lifecycle decay sweep uses so the two can never drift.
/// Semantic is the map's default bucket: absent kind reads as semantic
/// everywhere in the system, and non-Memory nodes (entities) deliberately
/// decay on the semantic curve too.
pub fn memory_kind_half_life() -> topodb::PropHalfLife {
    topodb::PropHalfLife {
        prop: MEMORY_KIND_PROP.to_string(),
        per_value: vec![
            (
                MEMORY_KIND_EPISODIC.to_string(),
                (LIFECYCLE_HALF_LIFE_EPISODIC_DAYS * DAY_MS) as i64,
            ),
            (
                MEMORY_KIND_PROCEDURAL.to_string(),
                (LIFECYCLE_HALF_LIFE_PROCEDURAL_DAYS * DAY_MS) as i64,
            ),
            // Numerically the default bucket today, but named explicitly:
            // the decision constant may diverge from semantic later, and an
            // explicit entry is what the mirror test can pin.
            (
                MEMORY_KIND_DECISION.to_string(),
                (LIFECYCLE_HALF_LIFE_DECISION_DAYS * DAY_MS) as i64,
            ),
        ],
        default_ms: (LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS * DAY_MS) as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ranking and the decay sweep must share one clock: the search half-life
    /// map is BUILT from the lifecycle constants, and semantic is deliberately
    /// the default bucket (absent kind == semantic everywhere else in the
    /// system, so it must not appear as an explicit entry that could drift).
    /// Every OTHER kind in the closed vocabulary gets an explicit entry —
    /// including `decision`, whose constant ties to semantic today but must
    /// stay independently pinned so the tie can be broken deliberately.
    /// The lifecycle sweep's decision half-life (LifecycleParams) is also
    /// pinned to the same constant, so editing LIFECYCLE_HALF_LIFE_DECISION_DAYS
    /// automatically updates both search ranking and decay candidates.
    #[test]
    fn memory_kind_half_life_mirrors_lifecycle_constants() {
        let map = memory_kind_half_life();
        assert_eq!(map.prop, MEMORY_KIND_PROP);
        assert_eq!(
            map.default_ms,
            (LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS * DAY_MS) as i64
        );
        assert_eq!(
            map.per_value,
            vec![
                (
                    MEMORY_KIND_EPISODIC.to_string(),
                    (LIFECYCLE_HALF_LIFE_EPISODIC_DAYS * DAY_MS) as i64
                ),
                (
                    MEMORY_KIND_PROCEDURAL.to_string(),
                    (LIFECYCLE_HALF_LIFE_PROCEDURAL_DAYS * DAY_MS) as i64
                ),
                (
                    MEMORY_KIND_DECISION.to_string(),
                    (LIFECYCLE_HALF_LIFE_DECISION_DAYS * DAY_MS) as i64
                ),
            ]
        );
        // The map's explicit entries plus the default bucket must cover the
        // closed vocabulary exactly — a fifth kind without a wiring decision
        // here is the drift this test exists to catch.
        let explicit: Vec<&str> = map.per_value.iter().map(|(k, _)| k.as_str()).collect();
        for kind in MEMORY_KINDS {
            assert!(
                explicit.contains(&kind) || kind == crate::MEMORY_KIND_DEFAULT,
                "kind {kind:?} has no half-life wiring"
            );
        }
        // The lifecycle sweep's decision half-life must also mirror the constant,
        // so the tie cannot drift between the two independent tables.
        let params = LifecycleParams::default();
        assert_eq!(
            params.half_life_decision_ms,
            (LIFECYCLE_HALF_LIFE_DECISION_DAYS * DAY_MS) as i64,
            "lifecycle sweep's decision half-life must equal LIFECYCLE_HALF_LIFE_DECISION_DAYS constant"
        );
        // And it must match what memory_kind_half_life() declares (the same constant).
        let map_decision_ms = map
            .per_value
            .iter()
            .find(|(k, _)| k == MEMORY_KIND_DECISION)
            .map(|(_, ms)| *ms)
            .expect("decision must have an explicit entry");
        assert_eq!(
            params.half_life_decision_ms, map_decision_ms,
            "lifecycle sweep and search ranking must use the same decision half-life"
        );
    }

    /// The 120d tie is deliberate and named: decision's constant equals
    /// semantic's, so a decision memory decays on the standing-fact curve
    /// until dogfood says otherwise.
    #[test]
    fn decision_half_life_ties_to_semantic() {
        assert_eq!(
            LIFECYCLE_HALF_LIFE_DECISION_DAYS,
            LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS
        );
    }
}
