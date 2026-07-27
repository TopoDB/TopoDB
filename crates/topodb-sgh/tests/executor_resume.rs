use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::cancel::CancelToken;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

/// Succeeds whatever node it's asked to run, but cancels `token` as a side
/// effect of that call — used to simulate a run interrupted between two
/// nodes without needing a real signal or a second thread: by the time the
/// executor's sequential loop reaches the *next* node in topological order,
/// its pre-execution cancellation check (in `Executor::run`, before
/// `execute_node` is ever called for that node) sees the token already
/// cancelled and marks that next node `Skipped` — never attempted, so it
/// carries no recorded attempts and its full budget is untouched.
struct SucceedThenCancel<'t> {
    token: &'t CancelToken,
}

impl AgentRunner for SucceedThenCancel<'_> {
    fn run(&self, _req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        self.token.cancel();
        Ok(NodeOutcome::Succeeded {
            output: "{}".to_string(),
        })
    }
}

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

/// `b` is interrupted before it ever gets a single attempt (simulated via
/// `SucceedThenCancel`, see its doc comment) — it lands in run 1 as
/// `Skipped`, not `Blocked`, with zero recorded attempts. This is
/// deliberately NOT the same shape as a node that exhausted its ladder (see
/// `recorded_attempts_consume_retry_budget` and
/// `cumulative_calls_across_resumes_stay_within_bound` for that case, where
/// a resume must make zero further calls): a never-attempted node has spent
/// none of its declared budget, so resume completing it with a fresh,
/// full-budget attempt is correct, not a bound violation.
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
    let cancel = CancelToken::new();
    let runner1 = SucceedThenCancel { token: &cancel };

    let mut ex1 = Executor::new(store, v, &runner1).with_cancel(cancel.clone());
    let report1 = ex1.run(10).unwrap();
    assert_eq!(report1.succeeded, vec!["a".to_string()]);
    assert_eq!(
        report1.skipped,
        vec!["b".to_string()],
        "b never got an attempt — cancellation landed between a and b"
    );
    assert!(
        ex1.store_ref().attempts("b").unwrap().is_empty(),
        "b has no recorded attempts to consume its budget"
    );

    let (store2, v2) = RunStore::open(&db, "r2").unwrap();
    let runner2 = MockRunner::new().script(
        "b",
        vec![NodeOutcome::Succeeded {
            output: "{}".into(),
        }],
    );

    // Fresh, uncancelled executor: resume proceeds normally.
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
    // Scripted to fail if called at all — the point of this test is that it
    // must NOT be called.
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
        0,
        "the node already spent its full bound reaching BLOCK in run 1 — a \
         resume must make no further model calls for it, or the published \
         bound is violated"
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

/// The general form of `recorded_attempts_consume_retry_budget`: a node
/// budgeted for `retries: 2, repairs: 0` (worst case = 3 calls: 1 initial +
/// 2 retries) blocks in its first run, then is resumed three more times in
/// three separate processes (each with its own fresh, counting runner). Every
/// resume must make zero calls — the node is done spending, permanently —
/// and the grand total across all four processes must equal exactly the
/// bound `worst_case` computed for this graph, never more.
#[test]
fn cumulative_calls_across_resumes_stay_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 2, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let bound = topodb_sgh::schema::bound::worst_case(&v);
    assert_eq!(bound.agent_calls, 3);

    let store = RunStore::create(&db, "r6", &v, 1).unwrap();
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
    assert_eq!(report1.model_calls, 3);

    let mut total_calls = report1.model_calls;
    let mut next_ms = 100;
    for i in 0..3 {
        let (store_n, v_n) = RunStore::open(&db, "r6").unwrap();
        let runner_n =
            MockRunner::new().script("a", vec![NodeOutcome::Failed { error: "z".into() }]);
        let mut ex_n = Executor::new(store_n, v_n, &runner_n);
        let report_n = ex_n.run(next_ms).unwrap();

        assert_eq!(
            runner_n.call_count(),
            0,
            "resume {i} must make zero calls: the bound was already spent in run 1"
        );
        assert_eq!(report_n.blocked, vec!["a".to_string()]);
        total_calls += report_n.model_calls;
        next_ms += 100;
    }

    assert_eq!(
        total_calls, bound.agent_calls,
        "cumulative model calls across the original run and every resume must \
         equal the worst-case bound, never exceed it"
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
