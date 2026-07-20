use std::collections::BTreeMap;

use topodb_sgh::runner::claude::{build_prompt, interpret_result};
use topodb_sgh::runner::{NodeOutcome, NodeRequest};

#[test]
fn prompt_contains_only_declared_inputs() {
    let mut inputs = BTreeMap::new();
    inputs.insert("survey".to_string(), r#"{"sites":[1]}"#.to_string());

    let req = NodeRequest {
        node_id: "build".into(),
        prompt: "Apply the edits".into(),
        inputs,
        output_schema: None,
    };

    let p = build_prompt(&req);
    assert!(p.contains("Apply the edits"));
    assert!(p.contains("survey"));
    assert!(p.contains(r#"{"sites":[1]}"#));
}

#[test]
fn prompt_demands_bare_json_when_a_schema_is_declared() {
    let req = NodeRequest {
        node_id: "a".into(),
        prompt: "Do the thing".into(),
        inputs: BTreeMap::new(),
        output_schema: Some(serde_json::json!({"type": "object"})),
    };

    let p = build_prompt(&req);
    assert!(
        p.contains("JSON"),
        "schema-bearing nodes must be told to emit JSON"
    );
    assert!(p.contains(r#""type""#), "the schema itself is included");
}

#[test]
fn prompt_omits_the_input_section_when_there_are_none() {
    let req = NodeRequest {
        node_id: "a".into(),
        prompt: "Start here".into(),
        inputs: BTreeMap::new(),
        output_schema: None,
    };
    let p = build_prompt(&req);
    assert!(!p.contains("## Inputs"));
}

// --- interpret_result -------------------------------------------------------
//
// `claude -p --output-format json` reports a blocked tool call in
// `permission_denials` while every other field still claims success:
// `subtype` stays "success", `is_error` stays false, and the process exits 0.
// A node whose Write was denied changed nothing, so treating that as success
// records a no-op as completed work.

fn denied_write() -> &'static str {
    r#"{
        "subtype": "success",
        "is_error": false,
        "result": "The Write tool call was blocked.",
        "permission_denials": [
            {"tool_name": "Write", "tool_use_id": "toolu_01", "tool_input": {"file_path": "/tmp/x"}}
        ]
    }"#
}

#[test]
fn a_denied_tool_is_a_failure_even_though_claude_reports_success() {
    match interpret_result(denied_write()) {
        NodeOutcome::Failed { .. } => {}
        NodeOutcome::Succeeded { output } => {
            panic!("a node whose Write was denied did no work, got success: {output}")
        }
    }
}

#[test]
fn a_denial_failure_names_the_tool_that_was_blocked() {
    match interpret_result(denied_write()) {
        NodeOutcome::Failed { error } => assert!(
            error.contains("Write"),
            "the error must name the denied tool so the cause is diagnosable, got: {error}"
        ),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn a_clean_run_yields_the_result_field_not_the_raw_json() {
    let json = r#"{
        "subtype": "success",
        "is_error": false,
        "result": "PONG",
        "permission_denials": []
    }"#;
    assert_eq!(
        interpret_result(json),
        NodeOutcome::Succeeded {
            output: "PONG".to_string()
        }
    );
}

#[test]
fn an_api_error_is_a_failure() {
    let json = r#"{
        "subtype": "error_during_execution",
        "is_error": true,
        "result": "overloaded",
        "permission_denials": []
    }"#;
    match interpret_result(json) {
        NodeOutcome::Failed { .. } => {}
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn unparseable_output_is_a_failure_rather_than_silent_success() {
    match interpret_result("not json at all") {
        NodeOutcome::Failed { .. } => {}
        other => panic!("unreadable output proves nothing about the work, got {other:?}"),
    }
}
