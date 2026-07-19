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
    let mut ex = Executor::new(store, v.clone(), &runner);
    let report = ex.run(10).unwrap();

    let ctx = collect_failure_context(ex.store_ref(), &v, &report).unwrap();
    assert_eq!(ctx.blocked, vec!["a".to_string()]);
    assert_eq!(ctx.skipped, vec!["b".to_string()]);
    assert!(
        ctx.attempts.iter().any(|(node, _, err)| node == "a" && err.contains("no such file")),
        "the failing node's error must be carried into the context: {:?}",
        ctx.attempts
    );
    assert_eq!(
        ctx.descriptions.get("a").map(String::as_str),
        Some("agent: p"),
        "the blocked node's description must be carried into the context: {:?}",
        ctx.descriptions
    );
}

#[test]
fn replan_goal_restates_the_original_and_the_failure() {
    // Use a node id that cannot occur incidentally in the surrounding prose,
    // unlike a single character such as "a" (which matches "approach",
    // "plan", "failed", etc. regardless of whether the id is ever rendered).
    let ctx = FailureContext {
        blocked: vec!["locate-config-file".into()],
        skipped: vec!["downstream-step".into()],
        attempts: vec![("locate-config-file".into(), "block".into(), "no such file".into())],
        descriptions: std::collections::BTreeMap::new(),
    };
    let goal = build_replan_goal("port the analyzer", &ctx);

    assert!(goal.contains("port the analyzer"), "the original goal must survive");
    assert!(goal.contains("locate-config-file"), "the blocked node must be named");
    assert!(goal.contains("no such file"), "the failure must be explained");
    assert!(
        goal.to_lowercase().contains("different"),
        "the planner must be told to try a different approach, not repeat the same graph"
    );
}

#[test]
fn replan_goal_includes_the_failed_node_s_description() {
    let mut descriptions = std::collections::BTreeMap::new();
    descriptions.insert("locate-config-file".to_string(), "agent: find the analyzer's config file in the repo root".to_string());
    let ctx = FailureContext {
        blocked: vec!["locate-config-file".into()],
        skipped: vec![],
        attempts: vec![("locate-config-file".into(), "block".into(), "no such file".into())],
        descriptions,
    };
    let goal = build_replan_goal("port the analyzer", &ctx);

    assert!(
        goal.contains("find the analyzer's config file in the repo root"),
        "the goal must describe what the failed step was trying to do, not merely its id: {goal}"
    );
}

#[test]
fn collect_failure_context_truncates_an_overlong_prompt() {
    let long_prompt = "x".repeat(500);
    let g = Graph::from_yaml(&format!(
        "version: 1\ngoal: port the analyzer\nnodes:\n\
         - {{id: a, kind: agent, prompt: \"{long_prompt}\", budget: {{retries: 0, repairs: 0}}}}\n"
    ))
    .unwrap();
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();

    let runner = MockRunner::new().script("a", vec![NodeOutcome::Failed { error: "no such file".into() }]);
    let mut ex = Executor::new(store, v.clone(), &runner);
    let report = ex.run(10).unwrap();

    let ctx = collect_failure_context(ex.store_ref(), &v, &report).unwrap();
    let desc = ctx.descriptions.get("a").expect("description recorded for blocked node");
    assert!(
        !desc.contains(&long_prompt),
        "a 500-char prompt must not be carried into the description verbatim: {desc}"
    );
    assert!(
        desc.len() < 500,
        "the description must be truncated to a short, readable length: {} chars",
        desc.len()
    );

    let goal = build_replan_goal(&v.graph.goal, &ctx);
    assert!(
        !goal.contains(&long_prompt),
        "a 500-char prompt must not be pasted into the replan goal verbatim"
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
        descriptions: std::collections::BTreeMap::new(),
    };

    let planner = MockPlanner::new(vec![Ok(
        "version: 1\ngoal: port the analyzer\nnodes:\n  - {id: locate, kind: agent, prompt: p, budget: {retries: 1, repairs: 0}}\n".to_string(),
    )]);

    let revised = propose_revision(&planner, &v, &ctx).expect("proposes");
    assert_eq!(revised.nodes.len(), 1);
    assert_eq!(revised.nodes[0].id, "locate");
    assert!(validate(&revised).is_ok(), "a proposal must itself be valid");
}
