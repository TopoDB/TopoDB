use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::planner::mock::MockPlanner;
use topodb_sgh::replan::{build_replan_goal, collect_failure_context, propose_revision, FailureContext};
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

fn chain() -> Graph {
    Graph::from_yaml(
        "version: 1\ngoal: port the analyzer\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap()
}

#[test]
fn failure_context_names_blocked_nodes_skipped_dependents_and_the_error() {
    let g = chain();
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    let runner = MockRunner::new().script("a", vec![NodeOutcome::Failed { error: "no such file".into() }]);
    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    let ctx = collect_failure_context(ex.store_ref(), &report).unwrap();
    assert_eq!(ctx.blocked, vec!["a".to_string()]);
    assert_eq!(ctx.skipped, vec!["b".to_string()]);
    assert!(
        ctx.attempts.iter().any(|(node, _, err)| node == "a" && err.contains("no such file")),
        "the failing node's error must be carried into the context: {:?}",
        ctx.attempts
    );
}

#[test]
fn replan_goal_restates_the_original_and_the_failure() {
    let ctx = FailureContext {
        blocked: vec!["a".into()],
        skipped: vec!["b".into()],
        attempts: vec![("a".into(), "block".into(), "no such file".into())],
    };
    let goal = build_replan_goal("port the analyzer", &ctx);

    assert!(goal.contains("port the analyzer"), "the original goal must survive");
    assert!(goal.contains('a'), "the blocked node must be named");
    assert!(goal.contains("no such file"), "the failure must be explained");
    assert!(
        goal.to_lowercase().contains("different"),
        "the planner must be told to try a different approach, not repeat the same graph"
    );
}

#[test]
fn propose_revision_returns_a_validated_successor_graph() {
    let g = chain();
    let v = validate(&g).unwrap();
    let ctx = FailureContext {
        blocked: vec!["a".into()],
        skipped: vec!["b".into()],
        attempts: vec![("a".into(), "block".into(), "no such file".into())],
    };

    let planner = MockPlanner::new(vec![Ok(
        "version: 1\ngoal: port the analyzer\nnodes:\n  - {id: locate, kind: agent, prompt: p, budget: {retries: 1, repairs: 0}}\n".to_string(),
    )]);

    let revised = propose_revision(&planner, &v, &ctx).expect("proposes");
    assert_eq!(revised.nodes.len(), 1);
    assert_eq!(revised.nodes[0].id, "locate");
    assert!(validate(&revised).is_ok(), "a proposal must itself be valid");
}
