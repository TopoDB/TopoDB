use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

/// A -> b two-node chain, both plain agent nodes.
fn chain() -> topodb_sgh::schema::validate::Validated {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    validate(&g).unwrap()
}

#[test]
fn a_succeeded_node_is_not_re_executed() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = chain();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner1 = MockRunner::new();

    let mut ex1 = Executor::new(store, v, &runner1);
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.succeeded, vec!["a".to_string(), "b".to_string()]);

    // Reopen the store by run id — resume path.
    let (store2, v2) = RunStore::open(&db, "r").unwrap();
    // A fresh runner scripted so BOTH nodes would fail (and thus block) if
    // they were re-executed.
    let runner2 = MockRunner::new()
        .script("a", vec![NodeOutcome::Failed { error: "x".into() }])
        .script("b", vec![NodeOutcome::Failed { error: "x".into() }]);

    let mut ex2 = Executor::new(store2, v2, &runner2);
    let report2 = ex2.run(20).unwrap();

    assert!(
        runner2.calls().is_empty(),
        "neither node should be re-executed: {:?}",
        runner2.calls()
    );
    assert_eq!(report2.succeeded, vec!["a".to_string(), "b".to_string()]);
}

/// Parallel-path variant of the same clause: with max_inflight(2), the
/// claim pass must also treat a Succeeded node as already-done rather than
/// spawning a worker for it.
#[test]
fn a_succeeded_node_is_not_re_executed_in_parallel_mode() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = chain();
    let store = RunStore::create(&db, "rp", &v, 1).unwrap();
    let runner1 = MockRunner::new();

    let mut ex1 = Executor::new(store, v, &runner1).with_max_inflight(2);
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.succeeded, vec!["a".to_string(), "b".to_string()]);

    let (store2, v2) = RunStore::open(&db, "rp").unwrap();
    let runner2 = MockRunner::new()
        .script("a", vec![NodeOutcome::Failed { error: "x".into() }])
        .script("b", vec![NodeOutcome::Failed { error: "x".into() }]);

    let mut ex2 = Executor::new(store2, v2, &runner2).with_max_inflight(2);
    let report2 = ex2.run(20).unwrap();

    assert!(
        runner2.calls().is_empty(),
        "neither node should be re-executed under the parallel scheduler: {:?}",
        runner2.calls()
    );
    assert_eq!(report2.succeeded, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn resume_completes_the_remaining_half() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r2", &v, 1).unwrap();
    let runner1 = MockRunner::new().script("b", vec![NodeOutcome::Failed { error: "x".into() }]);

    let mut ex1 = Executor::new(store, v, &runner1);
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.blocked, vec!["b".to_string()]);

    let (store2, v2) = RunStore::open(&db, "r2").unwrap();
    let runner2 = MockRunner::new().script(
        "b",
        vec![NodeOutcome::Succeeded {
            output: "{}".into(),
        }],
    );

    let mut ex2 = Executor::new(store2, v2, &runner2);
    let report2 = ex2.run(20).unwrap();

    assert_eq!(report2.succeeded, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(
        runner2.calls(),
        vec!["b".to_string()],
        "a is untouched, only b re-executes"
    );
}

#[test]
fn recorded_attempts_consume_retry_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 2, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r3", &v, 1).unwrap();
    let runner1 = MockRunner::new().script(
        "a",
        vec![
            NodeOutcome::Failed { error: "x".into() },
            NodeOutcome::Failed { error: "x".into() },
            NodeOutcome::Failed { error: "x".into() },
        ],
    );

    let mut ex1 = Executor::new(store, v, &runner1);
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.blocked, vec!["a".to_string()]);
    assert_eq!(
        runner1.call_count(),
        3,
        "initial attempt + 2 retries burns the whole budget"
    );

    let attempts_before = ex1.store_ref().attempts("a").unwrap();
    let retry_count_before = attempts_before
        .iter()
        .filter(|(rung, _)| rung == "retry")
        .count();
    assert_eq!(retry_count_before, 2, "both retries recorded");

    let (store2, v2) = RunStore::open(&db, "r3").unwrap();
    let runner2 = MockRunner::new().script("a", vec![NodeOutcome::Failed { error: "y".into() }]);

    let mut ex2 = Executor::new(store2, v2, &runner2);
    // Run 1's ladder (3 attempts across retries+block) advances the logical
    // clock well past 20 for this node's HAS_STATE edges; start run 2's own
    // logical clock past that so its writes are strictly later, matching the
    // real CLI's wall-clock invariant (see `link_superseding`'s monotonicity
    // requirement).
    let report2 = ex2.run(100).unwrap();

    assert_eq!(report2.blocked, vec!["a".to_string()]);
    assert_eq!(
        runner2.call_count(),
        1,
        "no retries left, so only one attempt happens before blocking again"
    );

    let attempts_after = ex2.store_ref().attempts("a").unwrap();
    let retry_count_after = attempts_after
        .iter()
        .filter(|(rung, _)| rung == "retry")
        .count();
    assert_eq!(
        retry_count_after, 2,
        "no new retry rung is recorded on resume — only the block"
    );
}

#[test]
fn an_approved_gate_passes_and_unblocks_deps() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: g, kind: gate, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [g], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r4", &v, 1).unwrap();
    let runner1 = MockRunner::new();

    let mut ex1 = Executor::new(store, v, &runner1);
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.blocked, vec!["g".to_string()]);
    assert_eq!(report1.skipped, vec!["b".to_string()]);
    assert!(runner1.calls().is_empty(), "b is skipped, never runs");

    let (store2, v2) = RunStore::open(&db, "r4").unwrap();
    store2.record_attempt("g", "approve", "", 15).unwrap();

    let runner2 = MockRunner::new();
    let mut ex2 = Executor::new(store2, v2, &runner2);
    let report2 = ex2.run(20).unwrap();

    assert!(report2.succeeded.contains(&"g".to_string()));
    assert!(report2.succeeded.contains(&"b".to_string()));
    assert_eq!(runner2.calls(), vec!["b".to_string()]);
    assert!(
        !report2.blocked_reasons.contains_key("g"),
        "an approved gate has no blocked_reasons entry"
    );
}

#[test]
fn an_unapproved_gate_still_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: g, kind: gate, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [g], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r5", &v, 1).unwrap();
    let runner1 = MockRunner::new();

    let mut ex1 = Executor::new(store, v, &runner1);
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.blocked, vec!["g".to_string()]);

    let (store2, v2) = RunStore::open(&db, "r5").unwrap();
    let runner2 = MockRunner::new();
    let mut ex2 = Executor::new(store2, v2, &runner2);
    let report2 = ex2.run(20).unwrap();

    assert_eq!(report2.blocked, vec!["g".to_string()]);
    assert_eq!(report2.skipped, vec!["b".to_string()]);
    assert!(runner2.calls().is_empty(), "b never runs without approval");
}
