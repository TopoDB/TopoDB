use topodb::{Db, PropValue, Scope, ScopeSet};
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::{NodeState, RunStore};
use topodb_sgh::store::EDGE_REVISION_OF;

fn store(db: &Db) -> RunStore {
    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();
    RunStore::create(db, "run-1", &v, 100).expect("create run")
}

#[test]
fn nodes_start_pending() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);
    assert_eq!(s.state("survey").unwrap(), NodeState::Pending);
    assert_eq!(s.state("build").unwrap(), NodeState::Pending);
}

#[test]
fn state_transitions_supersede_and_keep_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.set_state("survey", NodeState::Ready, 200).unwrap();
    s.set_state("survey", NodeState::Running, 300).unwrap();
    s.set_state("survey", NodeState::Succeeded, 400).unwrap();

    assert_eq!(s.state("survey").unwrap(), NodeState::Succeeded);

    // History is intact: as_of reads recover the past.
    assert_eq!(s.state_at("survey", 250).unwrap(), Some(NodeState::Ready));
    assert_eq!(s.state_at("survey", 350).unwrap(), Some(NodeState::Running));
}

#[test]
fn outputs_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    assert_eq!(s.output("survey").unwrap(), None);
    s.record_output("survey", r#"{"sites":[]}"#, 500).unwrap();
    assert_eq!(
        s.output("survey").unwrap().as_deref(),
        Some(r#"{"sites":[]}"#)
    );
}

#[test]
fn record_output_supersedes_prior_output() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_output("survey", r#"{"sites":[]}"#, 500).unwrap();
    s.record_output("survey", r#"{"sites":["a"]}"#, 600)
        .unwrap();

    assert_eq!(
        s.output("survey").unwrap().as_deref(),
        Some(r#"{"sites":["a"]}"#)
    );
}

#[test]
fn attempts_accumulate() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_attempt("survey", "retry", "timeout", 600).unwrap();
    s.record_attempt("survey", "repair", "schema mismatch", 700)
        .unwrap();
    assert_eq!(s.attempts("survey").unwrap().len(), 2);
}

/// `attempts()` used to read with `as_of: None` (wall clock) instead of the
/// same deterministic sentinel `state()` and `output()` use. That only
/// worked because every caller in this crate stamps timestamps well behind
/// the real wall clock (the CLI uses `now = 1`); a caller stamping a run
/// with a future-dated timestamp got an empty attempt history back with no
/// error, silently. Use a `now_ms` far in the future to prove the read is no
/// longer anchored to real wall time.
#[test]
fn attempts_are_visible_even_when_the_run_is_stamped_far_in_the_future() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();

    // Comfortably past real wall-clock time (ms since epoch), but still well
    // under the crate's `as_of` sentinel of `i64::MAX - 1`.
    let future = 4_102_444_800_000i64; // year 2100
    let s = RunStore::create(&db, "run-future", &v, future).expect("create run");

    s.record_attempt("survey", "retry", "timeout", future + 100)
        .unwrap();
    s.record_attempt("survey", "repair", "schema mismatch", future + 200)
        .unwrap();

    let attempts = s.attempts("survey").unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "attempts recorded with future timestamps must still be visible"
    );
}

#[test]
fn a_run_starts_with_no_revision() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);
    assert_eq!(s.revision().unwrap(), None);
}

#[test]
fn revisions_round_trip_and_supersede() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_revision("version: 1\ngoal: first\nnodes: []\n", "survey blocked", 500)
        .unwrap();
    let (yaml, reason) = s.revision().unwrap().expect("a revision exists");
    assert!(yaml.contains("first"));
    assert_eq!(reason, "survey blocked");

    s.record_revision("version: 1\ngoal: second\nnodes: []\n", "build blocked", 600)
        .unwrap();
    let (yaml, reason) = s.revision().unwrap().expect("still exactly one open revision");
    assert!(yaml.contains("second"), "the latest proposal wins");
    assert_eq!(reason, "build blocked");

    // Durability: the first, superseded proposal must still exist as a
    // closed REVISION_OF edge with its payload intact — supersession closes
    // the old edge, it never deletes or overwrites it.
    let scope_id = match s.scope() {
        Scope::Id(id) => id,
        Scope::Shared => panic!("run scope must be Scope::Id"),
    };
    let scopes = ScopeSet::of(&[scope_id]);
    let all = db
        .edges_from(&scopes, s.run_node(), None, Some(EDGE_REVISION_OF), false)
        .unwrap();
    assert_eq!(all.len(), 2, "both proposals survive as edges, none deleted");

    let open: Vec<_> = all.iter().filter(|e| e.valid_to.is_none()).collect();
    assert_eq!(open.len(), 1, "exactly one open revision edge");
    let open_rec = db.node(&scopes, open[0].to).expect("open revision node exists");
    match open_rec.props.get("yaml") {
        Some(PropValue::Str(s)) => assert!(s.contains("second"), "open edge points at the latest revision"),
        other => panic!("expected yaml prop, got {other:?}"),
    }

    let closed: Vec<_> = all.iter().filter(|e| e.valid_to.is_some()).collect();
    assert_eq!(closed.len(), 1, "exactly one closed (superseded) revision edge");
    assert_eq!(
        closed[0].valid_to,
        Some(600),
        "superseded edge closed at the second call's timestamp"
    );

    let superseded_rec = db.node(&scopes, closed[0].to).expect("superseded revision node exists");
    let superseded_yaml = match superseded_rec.props.get("yaml") {
        Some(PropValue::Str(s)) => s.clone(),
        other => panic!("expected yaml prop, got {other:?}"),
    };
    let superseded_reason = match superseded_rec.props.get("reason") {
        Some(PropValue::Str(s)) => s.clone(),
        other => panic!("expected reason prop, got {other:?}"),
    };
    assert!(
        superseded_yaml.contains("first"),
        "superseded revision's payload is still readable, not wiped"
    );
    assert_eq!(superseded_reason, "survey blocked");
}
