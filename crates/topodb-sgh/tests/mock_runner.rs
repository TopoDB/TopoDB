use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::{AgentRunner, NodeOutcome, NodeRequest};

fn req(id: &str) -> NodeRequest {
    NodeRequest {
        node_id: id.to_string(),
        prompt: "p".to_string(),
        inputs: Default::default(),
        output_schema: None,
    }
}

#[test]
fn returns_scripted_outcomes_in_order() {
    let r = MockRunner::new()
        .script("a", vec![
            NodeOutcome::Failed { error: "boom".into() },
            NodeOutcome::Succeeded { output: r#"{"ok":true}"#.into() },
        ]);

    assert!(matches!(r.run(&req("a")).unwrap(), NodeOutcome::Failed { .. }));
    assert!(matches!(r.run(&req("a")).unwrap(), NodeOutcome::Succeeded { .. }));
}

#[test]
fn unscripted_nodes_succeed_with_empty_object() {
    let r = MockRunner::new();
    match r.run(&req("z")).unwrap() {
        NodeOutcome::Succeeded { output } => assert_eq!(output, "{}"),
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn exhausted_script_repeats_its_last_outcome() {
    let r = MockRunner::new().script("a", vec![NodeOutcome::Failed { error: "always".into() }]);
    for _ in 0..5 {
        assert!(matches!(r.run(&req("a")).unwrap(), NodeOutcome::Failed { .. }));
    }
}

#[test]
fn records_a_call_log() {
    let r = MockRunner::new();
    r.run(&req("a")).unwrap();
    r.run(&req("b")).unwrap();
    assert_eq!(r.calls(), vec!["a".to_string(), "b".to_string()]);
}
