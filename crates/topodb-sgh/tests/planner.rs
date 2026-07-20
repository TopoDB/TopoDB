use topodb_sgh::planner::mock::MockPlanner;
use topodb_sgh::planner::{build_plan_prompt, PlanRequest, Planner, PlannerError};
use topodb_sgh::schema::validate::ValidationError;

fn req() -> PlanRequest {
    PlanRequest {
        goal: "port the analyzer".into(),
        context: None,
    }
}

#[test]
fn prompt_states_the_goal_and_teaches_the_schema() {
    let p = build_plan_prompt(&req(), &[], None);
    assert!(p.contains("port the analyzer"));
    assert!(p.contains("kind"), "must describe node kinds");
    assert!(p.contains("budget"), "budget is required on every node");
    assert!(p.contains("needs"), "must describe dependencies");
    assert!(
        p.contains("version: 1"),
        "must show the expected top-level shape"
    );
}

#[test]
fn prompt_teaches_output_schema_must_compile() {
    let p = build_plan_prompt(&req(), &[], None);
    assert!(
        p.contains("must be a valid JSON Schema document that compiles"),
        "must warn that an invalid output.schema rejects the whole graph"
    );
}

#[test]
fn prompt_requires_artifact_producing_agents_to_declare_an_output() {
    let p = build_plan_prompt(&req(), &[], None);
    assert!(
        p.contains("declare `output.schema`"),
        "an agent node that produces something must be told to declare what it produced"
    );
}

#[test]
fn prompt_tells_the_planner_a_self_report_is_not_verification() {
    let p = build_plan_prompt(&req(), &[], None);
    assert!(
        p.contains("is not evidence"),
        "the planner must be told an agent's own claim does not verify the work"
    );
    assert!(
        p.contains("kind: command"),
        "and that a command node is what actually checks a claim"
    );
}

#[test]
fn prompt_explains_the_gate_kind() {
    let p = build_plan_prompt(&req(), &[], None);
    assert!(
        p.contains("`kind: gate` halts the run before its dependents"),
        "must explain what a gate node does and when to use it"
    );
}

#[test]
fn prompt_includes_optional_context_when_given() {
    let r = PlanRequest {
        goal: "g".into(),
        context: Some("the tokenizer lives in crates/topodb/src/fts.rs".into()),
    };
    assert!(build_plan_prompt(&r, &[], None).contains("crates/topodb/src/fts.rs"));
}

#[test]
fn retry_prompt_feeds_back_the_errors_and_the_rejected_yaml() {
    let errs = vec![
        ValidationError::DanglingNeed {
            node: "b".into(),
            missing: "ghost".into(),
        },
        ValidationError::MissingPrompt("a".into()),
    ];
    let p = build_plan_prompt(&req(), &errs, Some("version: 1\ngoal: g\nnodes: []\n"));

    assert!(
        p.contains("ghost"),
        "the specific dangling id must be fed back"
    );
    assert!(
        p.contains("has no prompt"),
        "the specific error must be fed back"
    );
    assert!(
        p.contains("nodes: []"),
        "the rejected document must be shown"
    );
}

#[test]
fn mock_planner_returns_scripted_graphs_in_order() {
    let p = MockPlanner::new(vec![
        Ok("version: 1\ngoal: g\nnodes:\n  - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n".to_string()),
    ]);
    let g = p.plan(&req()).expect("plans");
    assert_eq!(g.nodes.len(), 1);
    assert_eq!(g.nodes[0].id, "a");
}

#[test]
fn mock_planner_surfaces_scripted_failures() {
    let p = MockPlanner::new(vec![Err("model unavailable".to_string())]);
    match p.plan(&req()) {
        Err(PlannerError::Runner(msg)) => assert!(msg.contains("unavailable")),
        other => panic!("expected a runner error, got {other:?}"),
    }
}

use std::sync::Mutex;
use topodb_sgh::planner::claude::{ClaudePlanner, PlanBackend};

/// A backend that returns scripted completions and records the prompts it saw.
struct ScriptedBackend {
    responses: Mutex<Vec<String>>,
    seen: Mutex<Vec<String>>,
}

impl ScriptedBackend {
    fn new(responses: Vec<&str>) -> Self {
        ScriptedBackend {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            seen: Mutex::new(Vec::new()),
        }
    }
    fn prompts(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl PlanBackend for ScriptedBackend {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError> {
        self.seen.lock().unwrap().push(prompt.to_string());
        let mut r = self.responses.lock().unwrap();
        if r.is_empty() {
            return Err(PlannerError::Runner("script exhausted".into()));
        }
        Ok(r.remove(0))
    }
}

const VALID: &str = "version: 1\ngoal: g\nnodes:\n  - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n";
const DANGLING: &str = "version: 1\ngoal: g\nnodes:\n  - {id: a, kind: agent, prompt: p, needs: [ghost], budget: {retries: 0, repairs: 0}}\n";

#[test]
fn a_valid_first_attempt_costs_exactly_one_backend_call() {
    let backend = ScriptedBackend::new(vec![VALID]);
    let p = ClaudePlanner::with_backend(Box::new(backend), 3);
    let g = p.plan(&req()).expect("plans");
    assert_eq!(g.nodes.len(), 1);
}

#[test]
fn an_invalid_attempt_is_retried_with_the_errors_fed_back() {
    let backend = std::sync::Arc::new(ScriptedBackend::new(vec![DANGLING, VALID]));
    let p = ClaudePlanner::with_backend(Box::new(backend.clone()), 3);

    let g = p.plan(&req()).expect("recovers on the second attempt");
    assert_eq!(g.nodes[0].id, "a");

    let prompts = backend.prompts();
    assert_eq!(prompts.len(), 2, "one initial attempt plus one retry");
    assert!(!prompts[0].contains("rejected"), "first prompt is clean");
    assert!(
        prompts[1].contains("ghost"),
        "retry must name the dangling dependency"
    );
}

#[test]
fn the_retry_loop_is_bounded_and_reports_exhaustion() {
    let backend = std::sync::Arc::new(ScriptedBackend::new(vec![
        DANGLING, DANGLING, DANGLING, DANGLING,
    ]));
    let p = ClaudePlanner::with_backend(Box::new(backend.clone()), 3);

    match p.plan(&req()) {
        Err(PlannerError::Exhausted { attempts, errors }) => {
            assert_eq!(attempts, 3, "must stop at max_attempts, not keep going");
            assert!(
                !errors.is_empty(),
                "the final validation errors must be reported"
            );
        }
        other => panic!("expected exhaustion, got {other:?}"),
    }
    assert_eq!(
        backend.prompts().len(),
        3,
        "exactly max_attempts backend calls, never more"
    );
}

#[test]
fn unparseable_yaml_is_retried_like_any_other_rejection() {
    let backend = std::sync::Arc::new(ScriptedBackend::new(vec![
        "this is not yaml: [unclosed",
        VALID,
    ]));
    let p = ClaudePlanner::with_backend(Box::new(backend.clone()), 3);
    assert!(p.plan(&req()).is_ok());
    assert_eq!(backend.prompts().len(), 2);
}
