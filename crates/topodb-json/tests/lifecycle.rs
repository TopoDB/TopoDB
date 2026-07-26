//! Behavioral tests for the Phase C lifecycle sweep (spec:
//! docs/superpowers/specs/2026-07-25-memory-lifecycle-design.md, Phase C).
use topodb::{Db, NodeId, Op, PropValue, Props, Scope, ScopeId, ScopeSet};
use topodb_json::{default_spec, lifecycle_candidates, staleness, ComposeError, LifecycleParams};

const DAY_MS: i64 = 86_400_000;

fn fresh_db(dir: &tempfile::TempDir) -> Db {
    Db::open_with(dir.path().join("t.redb"), default_spec()).unwrap()
}

fn memory(content: &str, kind: Option<&str>, scope: ScopeId) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    if let Some(k) = kind {
        props.insert("kind".into(), PropValue::Str(k.into()));
    }
    (
        id,
        Op::CreateNode {
            id,
            scope: Scope::Id(scope),
            label: "Memory".into(),
            props,
        },
    )
}

/// The pure formula: staleness = (age / half_life) / ln(e + access_count).
#[test]
fn staleness_scales_with_age_and_damps_with_access() {
    // Never accessed: ln(e) = 1, so staleness == age / half_life exactly.
    assert!((staleness(14 * DAY_MS, 14 * DAY_MS, 0) - 1.0).abs() < 1e-9);
    assert!((staleness(28 * DAY_MS, 14 * DAY_MS, 0) - 2.0).abs() < 1e-9);
    // Access damping: more hits, lower staleness, log-slow.
    let untouched = staleness(28 * DAY_MS, 14 * DAY_MS, 0);
    let touched = staleness(28 * DAY_MS, 14 * DAY_MS, 5);
    let hot = staleness(28 * DAY_MS, 14 * DAY_MS, 500);
    assert!(untouched > touched && touched > hot);
    assert!(hot > 0.0, "damping never zeroes a stale memory out");
    // Age clamp: a node minted after `now` reads as age 0.
    assert_eq!(staleness(-5, 14 * DAY_MS, 0), 0.0);
}

/// Kind selects the half-life: same age, episodic ≫ semantic ≫ procedural.
/// Absent and invalid kinds both degrade to semantic. Ordering is
/// deterministic (ties by ascending id) and the whole call is reproducible
/// under an injected now_ms.
#[test]
fn candidates_rank_by_kind_half_life_with_semantic_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let s = ScopeId::new();
    let (ep, op_a) = memory("episodic fact", Some("episodic"), s);
    let (se, op_b) = memory("semantic fact", None, s);
    let (pr, op_c) = memory("procedural fact", Some("procedural"), s);
    let (bad, op_d) = memory("mystery fact", Some("factual"), s);
    db.submit(vec![op_a, op_b, op_c, op_d]).unwrap();

    // All minted "now"; sweep from 28 days in the future.
    let now = ep.timestamp_ms() as i64 + 28 * DAY_MS;
    let scopes = ScopeSet::of(&[s]);
    let got = lifecycle_candidates(&db, &scopes, &LifecycleParams::default(), now).unwrap();

    assert_eq!(got.len(), 4);
    assert_eq!(
        got[0].id,
        ep.to_string(),
        "14d half-life is stalest at equal age"
    );
    assert_eq!(got[0].kind, "episodic");
    // Both semantic-effective candidates outrank procedural (120d < 365d),
    // and tie-break between them is ascending id.
    let mut sem_pair = vec![se.to_string(), bad.to_string()];
    sem_pair.sort();
    assert_eq!(
        vec![got[1].id.clone(), got[2].id.clone()],
        sem_pair,
        "absent and invalid kinds both use the semantic half-life; ties by id"
    );
    assert_eq!(got[1].kind, "semantic");
    assert_eq!(
        got[2].kind, "semantic",
        "invalid stored kind reports the effective kind"
    );
    assert_eq!(got[3].id, pr.to_string());

    // Evidence fields: mint time, never-accessed markers, formula value.
    assert_eq!(got[0].created_at, ep.timestamp_ms() as i64);
    assert_eq!(got[0].access_count, 0);
    assert_eq!(got[0].last_accessed_at, 0);
    assert_eq!(got[0].content, "episodic fact");
    let expect = staleness(now - ep.timestamp_ms() as i64, 14 * DAY_MS, 0);
    assert!((got[0].staleness - expect).abs() < 1e-9);

    // Determinism: the identical call yields the identical ranking.
    let again = lifecycle_candidates(&db, &scopes, &LifecycleParams::default(), now).unwrap();
    let ids: Vec<_> = got.iter().map(|c| c.id.clone()).collect();
    let ids2: Vec<_> = again.iter().map(|c| c.id.clone()).collect();
    assert_eq!(ids, ids2);
}

/// Tombstoned memories never surface; limit truncates AFTER ranking.
#[test]
fn candidates_skip_tombstoned_and_honor_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let s = ScopeId::new();
    let (live_a, op_a) = memory("alpha", Some("episodic"), s);
    let (live_b, op_b) = memory("beta", None, s);
    let (dead_sup, op_c) = memory("superseded one", None, s);
    let (dead_forg, op_d) = memory("forgotten one", None, s);
    db.submit(vec![op_a, op_b, op_c, op_d]).unwrap();
    let stamp = |id: NodeId, prop: &str| Op::SetNodeProps {
        id,
        props: [(prop.to_string(), Some(PropValue::Int(1_000)))]
            .into_iter()
            .collect(),
    };
    db.submit(vec![
        stamp(dead_sup, "superseded_at"),
        stamp(dead_forg, "forgotten_at"),
    ])
    .unwrap();

    let now = live_a.timestamp_ms() as i64 + 28 * DAY_MS;
    let scopes = ScopeSet::of(&[s]);
    let all = lifecycle_candidates(&db, &scopes, &LifecycleParams::default(), now).unwrap();
    assert_eq!(
        all.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        vec![live_a.to_string(), live_b.to_string()],
        "either tombstone excludes its memory from the sweep"
    );

    let one = lifecycle_candidates(
        &db,
        &scopes,
        &LifecycleParams {
            limit: 1,
            ..LifecycleParams::default()
        },
        now,
    )
    .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        one[0].id,
        live_a.to_string(),
        "limit keeps the STALEST, not the first found"
    );
}

/// The join with access_stats is real: a bumped node reports its count and
/// is damped below an identical never-accessed twin. The sweep itself must
/// never bump — proven with the counters fence idiom (no bare sleeps).
#[test]
fn candidates_join_access_stats_and_never_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (bumped, op_a) = memory("gamma twin", Some("episodic"), s);
    let (quiet, op_b) = memory("delta twin", Some("episodic"), s);
    let (fence, op_f) = memory("fence node", Some("episodic"), s);
    db.submit(vec![op_a, op_b, op_f]).unwrap();

    // Bump `bumped` once via a scoped read, then poll until the async
    // flusher lands it (bounded poll, same contract as counters.rs's
    // wait_for_count — no bare sleep).
    let _ = db.node(&scopes, bumped);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let stats = db.access_stats(&scopes, bumped).unwrap().unwrap();
        if stats.access_count >= 1 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "bump never flushed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let now = bumped.timestamp_ms() as i64 + 28 * DAY_MS;
    let got = lifecycle_candidates(&db, &scopes, &LifecycleParams::default(), now).unwrap();
    let by_id = |id: NodeId| got.iter().find(|c| c.id == id.to_string()).unwrap();
    assert_eq!(by_id(bumped).access_count, 1);
    assert!(by_id(bumped).last_accessed_at > 0);
    assert!(
        by_id(bumped).staleness < by_id(quiet).staleness,
        "an accessed memory is damped below its never-accessed twin"
    );

    // Unbumped proof: run a second sweep, then bump the FENCE via a real
    // read and wait for ITS count — once the fence's later bump has
    // flushed, any bump the sweep had issued would have flushed too.
    let _ = lifecycle_candidates(&db, &scopes, &LifecycleParams::default(), now).unwrap();
    let _ = db.node(&scopes, fence);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let stats = db.access_stats(&scopes, fence).unwrap().unwrap();
        if stats.access_count >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fence bump never flushed"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        db.access_stats(&scopes, quiet)
            .unwrap()
            .unwrap()
            .access_count,
        0,
        "two sweeps must not bump a scanned node"
    );
    assert_eq!(
        db.access_stats(&scopes, bumped)
            .unwrap()
            .unwrap()
            .access_count,
        1,
        "two sweeps must not bump beyond the one real read"
    );
}

/// Bad params reject before any db work.
#[test]
fn candidates_reject_zero_limit_and_nonpositive_half_lives() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    for (params, needle) in [
        (
            LifecycleParams {
                limit: 0,
                ..LifecycleParams::default()
            },
            "limit",
        ),
        (
            LifecycleParams {
                half_life_episodic_ms: 0,
                ..LifecycleParams::default()
            },
            "half-lives must be positive",
        ),
        (
            LifecycleParams {
                half_life_procedural_ms: -1,
                ..LifecycleParams::default()
            },
            "half-lives must be positive",
        ),
    ] {
        match lifecycle_candidates(&db, &scopes, &params, 1_000) {
            Err(ComposeError::Invalid(m)) => assert!(m.contains(needle), "{needle:?} not in {m:?}"),
            other => panic!("expected Invalid({needle}), got {other:?}"),
        }
    }
}
