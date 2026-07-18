use topodb::Db;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::{NodeState, RunStore};

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
