use std::collections::BTreeMap;

use topodb_sgh::runner::claude::build_prompt;
use topodb_sgh::runner::NodeRequest;

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
    assert!(p.contains("JSON"), "schema-bearing nodes must be told to emit JSON");
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
