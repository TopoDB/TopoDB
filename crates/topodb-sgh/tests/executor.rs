use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::validate::{validate, Validated};
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::{NodeState, RunStore};
use topodb_sgh::store::SghError;

fn diamond() -> Validated {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n\
         - {id: c, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n\
         - {id: d, kind: agent, prompt: p, needs: [b, c], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    validate(&g).unwrap()
}

#[test]
fn runs_every_node_in_topological_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = diamond();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.succeeded.len(), 4);
    assert_eq!(runner.calls().first().unwrap(), "a");
    assert_eq!(runner.calls().last().unwrap(), "d");
}

#[test]
fn a_failed_node_blocks_and_its_dependents_skip() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = diamond();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script("b", vec![NodeOutcome::Failed { error: "x".into() }]);

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["b".to_string()]);
    assert_eq!(report.skipped, vec!["d".to_string()], "d needs b");
    assert!(
        report.succeeded.contains(&"c".to_string()),
        "independent branch still runs"
    );
}

#[test]
fn declared_inputs_are_the_only_context_a_node_receives() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = diamond();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    // A runner that asserts on what it is handed.
    struct Spy;
    impl topodb_sgh::runner::AgentRunner for Spy {
        fn run(
            &self,
            req: &topodb_sgh::runner::NodeRequest,
        ) -> Result<NodeOutcome, topodb_sgh::runner::RunnerError> {
            if req.node_id == "d" {
                let keys: Vec<&String> = req.inputs.keys().collect();
                assert_eq!(keys, vec!["b", "c"], "d sees exactly its declared deps");
            }
            if req.node_id == "a" {
                assert!(req.inputs.is_empty(), "a has no deps and sees nothing");
            }
            Ok(NodeOutcome::Succeeded {
                output: "{}".into(),
            })
        }
    }

    let mut ex = Executor::new(store, v, &Spy);
    ex.run(10).unwrap();
}

#[test]
fn model_calls_counts_exactly_the_four_agent_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = diamond();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.succeeded.len(), 4);
    assert_eq!(
        report.model_calls, 4,
        "each of the 4 agent nodes makes exactly one model call"
    );
}

/// Command nodes have no execution path (see the executor module doc
/// comment): `Executor::run` must refuse a graph containing one before any
/// node executes, rather than dispatching it through `AgentRunner` as a
/// prompt. This used to be `model_calls_ignores_command_nodes`, which
/// asserted the command node ran (via `AgentRunner`) and simply didn't count
/// toward `model_calls` — pinning the exact defect this fix closes: the run
/// happened and the bound (which gives command nodes `0` toward
/// `agent_calls`) and the report disagreed about how many model calls
/// occurred.
#[test]
fn run_refuses_a_graph_containing_a_command_node() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: command, run: echo hi, needs: [a], budget: {retries: 0, repairs: 0}}\n\
         - {id: c, kind: agent, prompt: p, needs: [b], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();

    let mut ex = Executor::new(store, v, &runner);
    let err = ex.run(10).unwrap_err();

    match err {
        SghError::NoCommandRunner { nodes } => {
            assert_eq!(
                nodes,
                vec!["b".to_string()],
                "the command node is named in the error"
            )
        }
        other => panic!("expected NoCommandRunner, got {other:?}"),
    }
    // Refused before any node executes: no model calls happened at all, not
    // even for `a`, which topologically precedes the offending node.
    assert_eq!(
        runner.call_count(),
        0,
        "the run must be refused before any node executes"
    );
}

/// A graph with more than one command node names all of them, in
/// declaration order, not just the first offender.
#[test]
fn run_refuses_and_names_every_command_node() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: command, run: 'true', budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: command, run: 'true', budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();

    let mut ex = Executor::new(store, v, &runner);
    let err = ex.run(10).unwrap_err();

    match err {
        SghError::NoCommandRunner { nodes } => {
            assert_eq!(nodes, vec!["a".to_string(), "b".to_string()])
        }
        other => panic!("expected NoCommandRunner, got {other:?}"),
    }
}

#[test]
fn model_calls_excludes_a_gate_node() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: g, kind: gate, needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    // No interactive surface exists yet for gates: a gate node always
    // transitions straight to Blocked (see execute_node), so it never
    // dispatches to the runner and its dependents (there are none here)
    // would be skipped rather than run.
    assert_eq!(report.blocked, vec!["g".to_string()]);
    assert_eq!(
        report.model_calls, 1,
        "only node a's model call counts; the gate contributes 0"
    );
}

#[test]
fn schema_mismatch_is_a_failure() {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n  \
         - id: a\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n    \
         output:\n      schema:\n        type: object\n        required: [sites]\n  \
         - {id: check, kind: command, run: 'true', needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Succeeded {
            output: "{}".into(),
        }],
    );
    // `check` exists only so the graph validates (an agent declaring an output
    // must have a command downstream). It never runs here: `a`'s output fails
    // its schema, so `a` blocks and `check` is skipped.
    let commands = topodb_sgh::runner::command::MockCommandRunner::new();

    let mut ex = Executor::new(store, v, &runner).with_command_runner(&commands);
    let report = ex.run(10).unwrap();
    assert_eq!(
        report.blocked,
        vec!["a".to_string()],
        "missing required field fails the node"
    );
    assert_eq!(
        report.skipped,
        vec!["check".to_string()],
        "the downstream check is skipped when the claim it guards fails"
    );
    assert!(commands.calls().is_empty(), "no command ran");
}

#[test]
fn a_configured_command_runner_lets_command_nodes_execute() {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: command, run: 'true', needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    let agents = MockRunner::new();
    let commands = topodb_sgh::runner::command::MockCommandRunner::new();

    let mut ex = Executor::new(store, v, &agents).with_command_runner(&commands);
    let report = ex.run(10).unwrap();

    assert_eq!(report.succeeded, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(
        report.model_calls, 1,
        "only the agent node costs a model call"
    );
    assert_eq!(
        report.command_runs, 1,
        "the command node is counted separately"
    );
    assert_eq!(commands.calls(), vec!["b".to_string()]);
    assert_eq!(
        agents.calls(),
        vec!["a".to_string()],
        "the command never reaches the agent runner"
    );
}

#[test]
fn command_runs_stay_within_the_computed_bound_under_retries() {
    // retries: 2 -> bound allows 1 + 2 = 3 command runs.
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: b, kind: command, run: 'false', budget: {retries: 2, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let bound = topodb_sgh::schema::bound::worst_case(&v);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    let agents = MockRunner::new();
    let commands = topodb_sgh::runner::command::MockCommandRunner::new().script(
        "b",
        vec![NodeOutcome::Failed {
            error: "nope".into(),
        }],
    );

    let mut ex = Executor::new(store, v, &agents).with_command_runner(&commands);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["b".to_string()]);
    assert_eq!(report.command_runs, 3, "1 initial + 2 retries");
    assert!(report.command_runs <= bound.command_runs);
    assert_eq!(
        report.model_calls, 0,
        "a command node never costs a model call"
    );
}

#[test]
fn a_command_node_is_still_refused_without_a_command_runner() {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: b, kind: command, run: 'true', budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let agents = MockRunner::new();

    let mut ex = Executor::new(store, v, &agents);
    match ex.run(10) {
        Err(topodb_sgh::store::SghError::NoCommandRunner { nodes }) => {
            assert_eq!(nodes, vec!["b".to_string()]);
        }
        other => panic!("expected refusal without a command runner, got {other:?}"),
    }
    assert_eq!(agents.call_count(), 0, "nothing ran");
}

#[test]
fn a_blocked_node_reports_the_reason_it_failed() {
    // A single agent node that always fails. Its failure text must survive
    // into the report — a blocked id with no reason is the observability gap
    // that made a tool-denied node indistinguishable from any other block.
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Failed {
            error: "claude was denied WebFetch".into(),
        }],
    );

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    assert_eq!(
        report.blocked_reasons.get("a").map(String::as_str),
        Some("claude was denied WebFetch"),
        "the report must carry why a node blocked, not only that it did"
    );
}

#[test]
fn a_node_blocked_after_the_full_retry_repair_ladder_still_reports_its_reason() {
    // Mirrors an agent node like triage-and-fix: retries and repairs both
    // budgeted, so the block happens down the repair rung, not at the direct
    // Rung::Block. The reason must survive that path too.
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 2, repairs: 2}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Failed {
            error: "claude was denied Bash".into(),
        }],
    );

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    assert_eq!(
        report.blocked_reasons.get("a").map(String::as_str),
        Some("claude was denied Bash"),
        "a node blocked via the repair rung must still carry its reason"
    );
}

#[test]
fn a_retry_of_a_schema_node_feeds_back_the_error_and_demands_json() {
    // A schema-bearing agent node whose first reply is prose (fails output
    // validation) and whose second reply is valid JSON. With retries:1 the
    // node must recover, and — the point of the fix — the retry's prompt must
    // carry the previous failure and a demand for JSON, so a real model
    // changes its output instead of narrating the same prose again. Without
    // that, an idempotent re-run against a done workspace blocks forever on
    // prose.
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: 'do the thing', output: {schema: {type: object}}, budget: {retries: 1, repairs: 0}}\n\
         - {id: c, kind: command, run: 'true', needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![
            NodeOutcome::Succeeded {
                output: "The work is already done, nothing to change.".into(),
            },
            NodeOutcome::Succeeded {
                output: "{}".into(),
            },
        ],
    );
    let commands = topodb_sgh::runner::command::MockCommandRunner::new();

    let mut ex = Executor::new(store, v, &runner).with_command_runner(&commands);
    let report = ex.run(10).unwrap();

    assert_eq!(
        report.succeeded,
        vec!["a".to_string(), "c".to_string()],
        "a prose-first schema node must recover on retry"
    );

    let prompts = runner.prompts();
    assert_eq!(
        prompts.len(),
        2,
        "the node ran twice: first attempt + one retry"
    );
    assert_eq!(
        prompts[0], "do the thing",
        "the first attempt uses the node's prompt verbatim"
    );
    let retry = &prompts[1];
    assert!(
        retry.contains("do the thing"),
        "the retry keeps the original task, got: {retry}"
    );
    assert!(
        retry.to_lowercase().contains("json"),
        "the retry must demand JSON so the model stops narrating, got: {retry}"
    );
    assert!(
        retry.contains("not valid json") || retry.to_lowercase().contains("previous"),
        "the retry must feed back what went wrong, got: {retry}"
    );
}

/// A pre-cancelled run starts nothing: every node is skipped, none succeed,
/// and the report carries no model calls.
#[test]
fn a_cancelled_run_starts_no_nodes() {
    use topodb_sgh::runner::cancel::CancelToken;

    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();

    let token = CancelToken::new();
    token.cancel(); // Pre-cancel before run

    let mut ex = Executor::new(store, v, &runner).with_cancel(token);
    let report = ex.run(10).unwrap();

    assert!(
        report.succeeded.is_empty(),
        "no nodes should succeed when pre-cancelled"
    );
    assert_eq!(
        report.model_calls, 0,
        "no model calls should happen when pre-cancelled"
    );
    assert_eq!(
        report.skipped,
        vec!["a".to_string()],
        "the node should be skipped when pre-cancelled"
    );
}

/// Cancellation between ladder rungs blocks the node with reason "cancelled"
/// and records a "cancelled" attempt instead of burning the retry budget.
#[test]
fn cancellation_mid_ladder_blocks_with_cancelled_attempt() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use topodb_sgh::runner::cancel::CancelToken;

    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 3, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    // Custom runner that cancels the token then returns Failed.
    let token = CancelToken::new();
    let token_for_runner = token.clone();

    struct CancellingRunner {
        token: CancelToken,
        calls: AtomicUsize,
    }

    impl topodb_sgh::runner::AgentRunner for CancellingRunner {
        fn run(
            &self,
            _req: &topodb_sgh::runner::NodeRequest,
        ) -> Result<topodb_sgh::runner::NodeOutcome, topodb_sgh::runner::RunnerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.token.cancel();
            Ok(topodb_sgh::runner::NodeOutcome::Failed {
                error: "fail".into(),
            })
        }
    }

    let runner = CancellingRunner {
        token: token_for_runner.clone(),
        calls: AtomicUsize::new(0),
    };

    let mut ex = Executor::new(store, v, &runner).with_cancel(token);
    let report = ex.run(10).unwrap();

    assert_eq!(
        report.blocked,
        vec!["a".to_string()],
        "the node should be blocked due to cancellation"
    );
    assert_eq!(
        report.blocked_reasons.get("a").map(String::as_str),
        Some("cancelled"),
        "the blocked reason should be 'cancelled'"
    );

    let attempts = ex.store_ref().attempts("a").unwrap();
    assert!(
        attempts
            .iter()
            .any(|(rung, error)| rung == "cancelled" && error == "cancelled"),
        "should have a 'cancelled' attempt with error 'cancelled', got: {attempts:?}"
    );

    assert_eq!(
        runner.calls.load(Ordering::SeqCst),
        1,
        "cancellation must prevent the retry from executing"
    );
}

/// A Denied outcome climbs the ladder exactly like Failed, and the blocked
/// reason names the tool without any provider branding.
#[test]
fn denied_outcome_blocks_with_debranded_reason() {
    let graph = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n  - id: a\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n",
    )
    .unwrap();
    let v = validate(&graph).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Denied {
            tool: "Write".into(),
        }],
    );

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    let reason = &report.blocked_reasons["a"];
    assert!(
        reason.contains("provider denied tool Write"),
        "got: {reason}"
    );
    assert!(!reason.to_lowercase().contains("claude"), "got: {reason}");
}

/// With an injected wall clock, store timestamps are real epoch ms and
/// non-decreasing even when the clock stalls (monotone clamp).
#[test]
fn injected_clock_stamps_states_with_wall_time() {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    let graph = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n  - id: a\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n",
    )
    .unwrap();
    let v = validate(&graph).unwrap();
    let runner = MockRunner::new();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "wall", &v, 1_000_000).unwrap();

    // A fake clock that returns 1_000_000 forever (stalled clock).
    static NOW: AtomicI64 = AtomicI64::new(1_000_000);
    let clock: topodb_sgh::executor::ClockFn = Arc::new(|| NOW.load(Ordering::SeqCst));

    let mut ex = Executor::new(store, v, &runner).with_clock(clock);
    let report = ex.run(1_000_000).unwrap();

    assert_eq!(report.succeeded, vec!["a".to_string()]);

    // Before the run's own start time, the node had no state yet.
    assert_eq!(ex.store_ref().state_at("a", 999_999).unwrap(), None);

    // At the "latest" sentinel, the node is Succeeded — every transition
    // landed at or after 1_000_000 despite the stalled clock: the
    // monotone clamp held, with no panic and no store rejection from
    // descending or repeated timestamps.
    assert_eq!(
        ex.store_ref().state_at("a", i64::MAX - 1).unwrap(),
        Some(NodeState::Succeeded)
    );
}
