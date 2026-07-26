//! Behavioral tests for Phase E purge planning (spec:
//! docs/superpowers/specs/2026-07-25-memory-lifecycle-design.md, Phase E).
use topodb::{Db, NodeId, Op, PropValue, Props, Scope, ScopeId, ScopeSet};
use topodb_json::{default_spec, plan_purge, ComposeError};

fn fresh_db(dir: &tempfile::TempDir) -> Db {
    Db::open_with(dir.path().join("t.redb"), default_spec()).unwrap()
}

fn memory(content: &str, scope: ScopeId) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
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

fn stamp(id: NodeId, prop: &str, value: PropValue) -> Op {
    Op::SetNodeProps {
        id,
        props: [(prop.to_string(), Some(value))].into_iter().collect(),
    }
}

/// The qualification rule: ANY tombstone strictly older than the cutoff
/// qualifies; the boundary value survives; live nodes and non-Int marks
/// are never touched. Ids come back ascending.
#[test]
fn plan_purge_selects_strictly_older_tombstones_of_either_kind() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let s = ScopeId::new();
    let (old_sup, op_a) = memory("old superseded", s);
    let (old_forg, op_b) = memory("old forgotten", s);
    let (boundary, op_c) = memory("boundary forgotten", s);
    let (fresh_t, op_d) = memory("freshly forgotten", s);
    let (live, op_e) = memory("live fact", s);
    let (weird, op_f) = memory("non-int mark", s);
    db.submit(vec![op_a, op_b, op_c, op_d, op_e, op_f]).unwrap();
    db.submit(vec![
        stamp(old_sup, "superseded_at", PropValue::Int(1_000)),
        stamp(old_forg, "forgotten_at", PropValue::Int(1_500)),
        stamp(boundary, "forgotten_at", PropValue::Int(2_000)),
        stamp(fresh_t, "forgotten_at", PropValue::Int(3_000)),
        stamp(weird, "forgotten_at", PropValue::Str("yesterday".into())),
    ])
    .unwrap();

    let scopes = ScopeSet::of(&[s]);
    let (ops, ids) = plan_purge(&db, &scopes, 2_000).unwrap();

    let mut expect = vec![old_sup.to_string(), old_forg.to_string()];
    expect.sort();
    assert_eq!(
        ids, expect,
        "strictly-older superseded_at OR forgotten_at; boundary (== cutoff) survives"
    );
    assert_eq!(ops.len(), ids.len());
    for (op, id) in ops.iter().zip(&ids) {
        match op {
            Op::RemoveNode { id: op_id } => {
                assert_eq!(&op_id.to_string(), id, "ops and ids align in order")
            }
            other => panic!("expected RemoveNode, got {other:?}"),
        }
    }

    // A huge cutoff still never touches live or non-Int-marked nodes.
    let (_, all_ids) = plan_purge(&db, &scopes, i64::MAX).unwrap();
    assert!(
        !all_ids.contains(&live.to_string()),
        "live memory never purged"
    );
    assert!(
        !all_ids.contains(&weird.to_string()),
        "non-Int tombstone value is not a mark"
    );
    assert_eq!(
        all_ids.len(),
        4,
        "everything Int-tombstoned qualifies under i64::MAX"
    );
}

/// Submitting the planned ops actually removes the nodes (and only them):
/// the plan is real, and re-planning after the purge is empty.
#[test]
fn submitted_purge_ops_remove_exactly_the_planned_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let s = ScopeId::new();
    let (doomed, op_a) = memory("doomed", s);
    let (survivor, op_b) = memory("survivor", s);
    db.submit(vec![op_a, op_b]).unwrap();
    db.submit(vec![stamp(doomed, "forgotten_at", PropValue::Int(1_000))])
        .unwrap();

    let scopes = ScopeSet::of(&[s]);
    let (ops, ids) = plan_purge(&db, &scopes, 2_000).unwrap();
    assert_eq!(ids, vec![doomed.to_string()]);
    db.submit(ops).unwrap();

    assert!(db.node(&scopes, doomed).is_none(), "purged node is gone");
    assert!(db.node(&scopes, survivor).is_some(), "survivor untouched");
    let (ops2, ids2) = plan_purge(&db, &scopes, 2_000).unwrap();
    assert!(
        ops2.is_empty() && ids2.is_empty(),
        "purge is idempotent — nothing left to plan"
    );
}

/// Bad cutoffs reject before any db work.
#[test]
fn plan_purge_rejects_nonpositive_cutoffs() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let scopes = ScopeSet::of(&[ScopeId::new()]);
    for cutoff in [0, -5] {
        match plan_purge(&db, &scopes, cutoff) {
            Err(ComposeError::Invalid(m)) => {
                assert!(
                    m.contains("tombstoned-before"),
                    "message names the flag: {m:?}"
                )
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
