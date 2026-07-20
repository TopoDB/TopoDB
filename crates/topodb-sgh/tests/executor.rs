use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::validate::{validate, Validated};
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;
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
