use topodb_sgh::schema::validate::{validate, ValidationError};
use topodb_sgh::schema::Graph;

fn graph(yaml: &str) -> Graph {
    Graph::from_yaml(yaml).expect("fixture parses")
}

#[test]
fn accepts_a_valid_graph() {
    let g = graph(include_str!("fixtures/simple.yaml"));
    let v = validate(&g).expect("valid");
    assert_eq!(
        v.topo_order,
        vec!["survey".to_string(), "build".to_string()]
    );
}

#[test]
fn rejects_a_cycle() {
    let g = graph(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: command, run: 'true', needs: [b], budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: command, run: 'true', needs: [a], budget: {retries: 0, repairs: 0}}\n",
    );
    let errs = validate(&g).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| matches!(e, ValidationError::Cycle { .. })));
}

#[test]
fn rejects_a_dangling_dependency() {
    let g = graph(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: command, run: 'true', needs: [ghost], budget: {retries: 0, repairs: 0}}\n",
    );
    let errs = validate(&g).unwrap_err();
    assert!(errs.iter().any(|e| matches!(
        e, ValidationError::DanglingNeed { node, missing } if node == "a" && missing == "ghost"
    )));
}

#[test]
fn rejects_duplicate_ids() {
    let g = graph(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: command, run: 'true', budget: {retries: 0, repairs: 0}}\n\
         - {id: a, kind: command, run: 'true', budget: {retries: 0, repairs: 0}}\n",
    );
    let errs = validate(&g).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| matches!(e, ValidationError::DuplicateId(id) if id == "a")));
}

#[test]
fn rejects_agent_without_prompt_and_command_without_run() {
    let g = graph(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: command, budget: {retries: 0, repairs: 0}}\n",
    );
    let errs = validate(&g).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| matches!(e, ValidationError::MissingPrompt(id) if id == "a")));
    assert!(errs
        .iter()
        .any(|e| matches!(e, ValidationError::MissingRun(id) if id == "b")));
}

/// A duplicated `needs` entry is redundant, not a structural error: the
/// dependency is still declared exactly once in substance. Before this fix,
/// indegree was incremented once per `needs` *occurrence* but decremented
/// only once per dependent node on removal, so the counter for a node with
/// `needs: [a, a]` never reached zero and the graph was rejected with a
/// `Cycle` error that named a node with no cycle at all — an actively
/// misleading diagnostic for a perfectly valid graph. Chosen resolution:
/// accept duplicates silently (treat as a single dependency) rather than add
/// a new diagnostic for them, since a repeated `needs` entry carries no
/// information a validator needs to reject.
#[test]
fn duplicate_needs_entry_validates_successfully_rather_than_reporting_a_cycle() {
    let g = graph(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: command, run: 'true', budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: command, run: 'true', needs: [a, a], budget: {retries: 0, repairs: 0}}\n",
    );
    let v = validate(&g).expect("a duplicated needs entry must not be reported as a cycle");
    assert_eq!(v.topo_order, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn rejects_malformed_output_schema() {
    let g = graph(
        "version: 1\n\
         goal: g\n\
         nodes:\n\
         \x20\x20- id: a\n\
         \x20\x20\x20\x20kind: agent\n\
         \x20\x20\x20\x20prompt: p\n\
         \x20\x20\x20\x20budget: {retries: 0, repairs: 0}\n\
         \x20\x20\x20\x20output:\n\
         \x20\x20\x20\x20\x20\x20schema: {type: 12}\n",
    );
    let errs = validate(&g).unwrap_err();
    assert!(errs
        .iter()
        .any(|e| matches!(e, ValidationError::InvalidSchema { .. })));
}
