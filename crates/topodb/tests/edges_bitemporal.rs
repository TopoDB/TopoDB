//! Belief-axis (recorded_at / superseded_at) write-path semantics.
use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec::default()
}

const JUNE: i64 = 1_780_300_800_000; // 2026-06-01T00:00:00Z
const AUGUST: i64 = 1_785_542_400_000; // 2026-08-01T00:00:00Z
const SEPT: i64 = 1_788_220_800_000; // 2026-09-01T00:00:00Z

// Derivation note (Task 3): AUGUST/SEPT above are exact UTC midnights of the
// 1st of their month (1_785_542_400_000 and 1_788_220_800_000 both divide
// evenly by 86_400_000, the length of a day in ms). JUNE does NOT follow that
// pattern — it is 1_780_300_800_000, which is 2026-06-01T08:00:00Z, not
// T00:00:00Z as its comment claims (a pre-existing Task-1 rounding quirk,
// harmless because every assertion only cares about JUNE's position relative
// to the other constants, not its literal wall-clock meaning). The task
// brief proposed JULY = 1_782_950_400_000 for this file, which is actually
// 2026-07-02T00:00:00Z (one day off) per the same exact-UTC-midnight scheme
// AUGUST/SEPT follow. Rather than propagate that off-by-one, JULY below is
// computed the same way AUGUST/SEPT were (exact UTC midnight of the 1st),
// which is all a query point between JUNE and AUGUST needs to be.
const JULY: i64 = 1_782_864_000_000; // 2026-07-01T00:00:00Z
                                     // A point strictly between AUGUST (world close) and SEPT (belief close) for
                                     // the divergent-close scenario, same exact-midnight scheme.
const MID_AUG: i64 = 1_786_752_000_000; // 2026-08-15T00:00:00Z
                                        // A point strictly after SEPT (belief close), same scheme.
const OCT: i64 = 1_790_812_800_000; // 2026-10-01T00:00:00Z

/// Point-in-time gates mirroring `topodb_json::edge_live_at`/
/// `edge_believed_at` exactly (this crate can't depend on topodb-json — it
/// depends on this crate — so the same two-line predicate is inlined here;
/// `edges_from` itself takes no `as_of`, only `axis` gating what `open_only`
/// means, so a caller wanting a point-in-time answer out of `edges_from`
/// post-filters the fetched record on these fields, same as the MCP surface
/// does today for the valid axis).
fn valid_at(rec: &EdgeRecord, t: i64) -> bool {
    rec.valid_from <= t && rec.valid_to.is_none_or(|vt| vt > t)
}
fn believed_at(rec: &EdgeRecord, t: i64) -> bool {
    rec.recorded_at <= t && rec.superseded_at.is_none_or(|st| st > t)
}

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
        .edges_from(&ScopeSet::of(&[s]), a, None, None, false, TimeAxis::Valid)
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
        .edges_from(&ScopeSet::of(&[s]), a, None, None, false, TimeAxis::Valid)
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

/// The late-recorded fact, both axes: valid-axis July sees it (world truth
/// since June); recorded-axis July does NOT (we had not written it yet);
/// recorded-axis September does. Asserted via both `traverse` (the
/// `TimeAxis`-gated hop) and `edges_from` (post-filtered on the fetched
/// record, mirroring the MCP `as_of` pattern).
#[test]
fn late_recorded_fact_answers_differ_by_axis() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
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
            valid_from: Some(JUNE), // world: true since June
            recorded_at: None,
        }],
        AUGUST, // belief: not written until August
    )
    .unwrap();

    // --- traverse ---
    let base = TraversalQuery {
        scopes: scopes.clone(),
        seeds: vec![a],
        max_hops: 1,
        edge_types: None,
        direction: Direction::Out,
        as_of: None,
        time_axis: TimeAxis::Valid,
    };
    let hits = |q: &TraversalQuery| db.traverse(q).unwrap().edges.iter().any(|r| r.id == e);

    assert!(
        hits(&TraversalQuery {
            as_of: Some(JULY),
            time_axis: TimeAxis::Valid,
            ..base.clone()
        }),
        "valid axis at July: world truth already held since June"
    );
    assert!(
        !hits(&TraversalQuery {
            as_of: Some(JULY),
            time_axis: TimeAxis::Recorded,
            ..base.clone()
        }),
        "recorded axis at July: not written until August"
    );
    assert!(
        hits(&TraversalQuery {
            as_of: Some(SEPT),
            time_axis: TimeAxis::Recorded,
            ..base.clone()
        }),
        "recorded axis at September: written by then"
    );

    // --- edges_from (open_only=false: never closed, so the axis parameter
    // itself is a no-op here; the point-in-time gate is a post-filter on the
    // fetched record, same shape as the traverse gate above) ---
    let rec = db
        .edges_from(&scopes, a, None, None, false, TimeAxis::Valid)
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        valid_at(&rec, JULY),
        "edges_from parity: valid axis at July"
    );
    assert!(
        !believed_at(&rec, JULY),
        "edges_from parity: recorded axis at July"
    );
    assert!(
        believed_at(&rec, SEPT),
        "edges_from parity: recorded axis at September"
    );
}

/// After a divergent close, each axis answers independently: the world says
/// the edge ended in August (valid-axis mid-August query → absent) while
/// belief says we still believed it until September (recorded-axis
/// mid-August query → present; recorded-axis October → absent). All four
/// axis/as_of combinations asserted via `edges_from`; the two mid-August
/// combinations also mirrored via `traverse`.
#[test]
fn divergent_close_answers_differ_by_axis() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
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
            superseded_at: None,
        }],
        SEPT, // belief: we learned (closed) in September
    )
    .unwrap();

    // --- edges_from: all four axis/as_of combinations ---
    let rec = db
        .edges_from(&scopes, a, None, None, false, TimeAxis::Valid)
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        !valid_at(&rec, MID_AUG),
        "valid axis mid-August: world says gone since August"
    );
    assert!(
        !valid_at(&rec, OCT),
        "valid axis October: still gone in the world"
    );
    assert!(
        believed_at(&rec, MID_AUG),
        "recorded axis mid-August: not superseded (in belief) until September"
    );
    assert!(
        !believed_at(&rec, OCT),
        "recorded axis October: superseded by September"
    );

    // --- traverse mirror of the two mid-August cases ---
    let base = TraversalQuery {
        scopes,
        seeds: vec![a],
        max_hops: 1,
        edge_types: None,
        direction: Direction::Out,
        as_of: Some(MID_AUG),
        time_axis: TimeAxis::Valid,
    };
    let hits = |q: &TraversalQuery| db.traverse(q).unwrap().edges.iter().any(|r| r.id == e);

    assert!(
        !hits(&TraversalQuery {
            time_axis: TimeAxis::Valid,
            ..base.clone()
        }),
        "traverse, valid axis mid-August: world says gone"
    );
    assert!(
        hits(&TraversalQuery {
            time_axis: TimeAxis::Recorded,
            ..base.clone()
        }),
        "traverse, recorded axis mid-August: still believed"
    );
}
