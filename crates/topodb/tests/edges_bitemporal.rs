//! Belief-axis (recorded_at / superseded_at) write-path semantics.
use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec::default()
}

const JUNE: i64 = 1_780_300_800_000; // 2026-06-01T00:00:00Z
const AUGUST: i64 = 1_785_542_400_000; // 2026-08-01T00:00:00Z
const SEPT: i64 = 1_788_220_800_000; // 2026-09-01T00:00:00Z

fn two_nodes(db: &Db, s: ScopeId) -> (NodeId, NodeId) {
    let a = NodeId::new();
    let b = NodeId::new();
    let mk = |id| Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "Entity".into(),
        props: Props::new(),
    };
    db.submit(vec![mk(a), mk(b)]).unwrap();
    (a, b)
}

/// A late-recorded fact: valid_from backdated to June, written "in August"
/// (deterministic now via submit_at). recorded_at must be the WRITE instant,
/// not the backdated world time, and must ignore any caller-supplied value.
#[test]
fn recorded_at_is_the_write_instant_never_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let (a, b) = two_nodes(&db, s);
    let e = EdgeId::new();
    db.submit_at(
        vec![Op::CreateEdge {
            id: e,
            scope: Scope::Id(s),
            ty: "works_at".into(),
            from: a,
            to: b,
            props: Props::new(),
            valid_from: Some(JUNE),
            recorded_at: Some(1), // hostile caller value — must be overwritten
        }],
        AUGUST,
    )
    .unwrap();
    let rec = db
        .edges_from(&ScopeSet::of(&[s]), a, None, None, false)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(rec.valid_from, JUNE, "world time honors the caller");
    assert_eq!(rec.recorded_at, AUGUST, "belief time is the write instant");
    assert_eq!(rec.superseded_at, None);
}

/// Closing stamps superseded_at at the OPERATION instant while valid_to
/// honors the caller's world-time override — the two axes diverge.
#[test]
fn close_diverges_the_axes_with_a_backdated_valid_to() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let (a, b) = two_nodes(&db, s);
    let e = EdgeId::new();
    db.submit_at(
        vec![Op::CreateEdge {
            id: e,
            scope: Scope::Id(s),
            ty: "works_at".into(),
            from: a,
            to: b,
            props: Props::new(),
            valid_from: Some(JUNE),
            recorded_at: None,
        }],
        JUNE,
    )
    .unwrap();
    db.submit_at(
        vec![Op::CloseEdge {
            id: e,
            valid_to: Some(AUGUST), // world: ended in August
            superseded_at: Some(2), // hostile — must be overwritten
        }],
        SEPT, // belief: we learned in September
    )
    .unwrap();
    let rec = db
        .edges_from(&ScopeSet::of(&[s]), a, None, None, false)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(rec.valid_to, Some(AUGUST));
    assert_eq!(rec.superseded_at, Some(SEPT));

    // Same assertion via the raw-storage debug dump — this is the read path
    // Task 2's migration parity checks will trust, and it must be pinned by
    // a DIVERGENT (non-copy-rule) fixture: valid_to (AUGUST) != superseded_at
    // (SEPT) here, so a decoder that silently fell back to the copy rule
    // (recorded_at/superseded_at derived from valid_from/valid_to, as v3 rows
    // must) would be caught red-handed instead of accidentally matching.
    let dumped = db
        .debug_dump_edges()
        .into_iter()
        .find(|r| r.id == e)
        .unwrap();
    assert_eq!(dumped.valid_from, JUNE);
    assert_eq!(dumped.valid_to, Some(AUGUST));
    assert_eq!(
        dumped.recorded_at, JUNE,
        "recorded_at from the create, unaffected by close"
    );
    assert_eq!(dumped.superseded_at, Some(SEPT));
}
