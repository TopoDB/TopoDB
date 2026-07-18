use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::validate::{validate, Validated};
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

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
    assert!(report.succeeded.contains(&"c".to_string()), "independent branch still runs");
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
            Ok(NodeOutcome::Succeeded { output: "{}".into() })
        }
    }

    let mut ex = Executor::new(store, v, &Spy);
    ex.run(10).unwrap();
}

#[test]
fn schema_mismatch_is_a_failure() {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n  \
         - id: a\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n    \
         output:\n      schema:\n        type: object\n        required: [sites]\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new()
        .script("a", vec![NodeOutcome::Succeeded { output: "{}".into() }]);

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();
    assert_eq!(report.blocked, vec!["a".to_string()], "missing required field fails the node");
}
