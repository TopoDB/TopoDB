use std::collections::BTreeMap;

use topodb_sgh::runner::claude::{build_prompt, extract_json, interpret_result};
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
    match interpret_result(denied_write(), false) {
        NodeOutcome::Failed { .. } => {}
        NodeOutcome::Succeeded { output } => {
            panic!("a node whose Write was denied did no work, got success: {output}")
        }
    }
}

#[test]
fn a_denial_failure_names_the_tool_that_was_blocked() {
    match interpret_result(denied_write(), false) {
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
        interpret_result(json, false),
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
    match interpret_result(json, false) {
        NodeOutcome::Failed { .. } => {}
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn unparseable_output_is_a_failure_rather_than_silent_success() {
    match interpret_result("not json at all", false) {
        NodeOutcome::Failed { .. } => {}
        other => panic!("unreadable output proves nothing about the work, got {other:?}"),
    }
}

// --- extract_json + expects_json ---------------------------------------------
//
// A schema-bearing agent node must reply with JSON. In practice the model
// intermittently wraps that JSON in a ```json fence or a sentence of prose,
// even when told not to — observed directly: the same node that emitted bare
// `{"tests_added":7,...}` on one call narrated prose on a re-run and blocked.
// Unwrapping the two common deviations turns a spurious block into a success;
// a reply with no JSON object at all still fails honestly.

fn envelope(result_json_escaped: &str) -> String {
    format!(
        r#"{{"subtype":"success","is_error":false,"permission_denials":[],"result":{}}}"#,
        result_json_escaped
    )
}

#[test]
fn extract_json_returns_bare_object_unchanged() {
    assert_eq!(extract_json(r#"{"a":1}"#).as_deref(), Some(r#"{"a":1}"#));
}

#[test]
fn extract_json_unwraps_a_fenced_block() {
    let s = "```json\n{\"a\":1}\n```";
    assert_eq!(extract_json(s).as_deref(), Some("{\"a\":1}"));
}

#[test]
fn extract_json_pulls_an_object_out_of_surrounding_prose() {
    let s = "The work is already complete. {\"tests_added\": 7} — nothing else to do.";
    assert_eq!(extract_json(s).as_deref(), Some("{\"tests_added\": 7}"));
}

#[test]
fn extract_json_finds_nothing_in_pure_prose() {
    assert_eq!(
        extract_json("Everything looks fine, no changes needed."),
        None
    );
}

#[test]
fn extract_json_ignores_an_unbalanced_brace() {
    // A stray '{' with no valid object must not be mistaken for JSON.
    assert_eq!(extract_json("cost was ${5 and rising"), None);
}

#[test]
fn interpret_result_unwraps_wrapped_json_when_json_is_expected() {
    let result =
        serde_json::to_string("Here is the result:\n```json\n{\"tests_added\":7}\n```").unwrap();
    let env = envelope(&result);
    assert_eq!(
        interpret_result(&env, true),
        NodeOutcome::Succeeded {
            output: "{\"tests_added\":7}".to_string()
        },
        "wrapped JSON from a schema node must be unwrapped, not blocked"
    );
}

#[test]
fn interpret_result_leaves_prose_alone_when_no_json_is_expected() {
    // A node with no output schema legitimately returns prose (e.g. a survey).
    // It must never be mangled by extraction.
    let result = serde_json::to_string("A prose summary { with a brace } inside.").unwrap();
    let env = envelope(&result);
    assert_eq!(
        interpret_result(&env, false),
        NodeOutcome::Succeeded {
            output: "A prose summary { with a brace } inside.".to_string()
        }
    );
}
