#![cfg(feature = "anthropic")]
use serde_json::{json, Value};
use topodb_sgh::provider::anthropic::AnthropicProvider;
use topodb_sgh::provider::*;

fn provider() -> AnthropicProvider {
    AnthropicProvider::new("k-test".into(), Some("claude-haiku-4-5".into()))
}

fn turn_with(parts: Vec<ContentPart>, tools: Vec<ToolDef>, schema: Option<Value>) -> ChatTurn {
    ChatTurn {
        model: None,
        system: Some("sys".into()),
        messages: vec![ChatMessage { role: Role::User, parts }],
        tools,
        output_schema: schema,
        max_tokens: 8192,
    }
}

#[test]
fn request_maps_text_tools_and_schema() {
    let turn = turn_with(
        vec![ContentPart::Text { text: "do it".into() }],
        vec![ToolDef { name: "topodb__search".into(), description: "d".into(), input_schema: json!({"type":"object"}) }],
        Some(json!({"type":"object","required":["n"]})),
    );
    let p = provider().request(&turn).unwrap();
    assert_eq!(p.url, "https://api.anthropic.com/v1/messages");
    assert!(p.headers.iter().any(|(k, v)| k == "x-api-key" && v == "k-test"));
    assert!(p.headers.iter().any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"));
    let body: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(body["model"], "claude-haiku-4-5");
    assert_eq!(body["system"], "sys");
    assert_eq!(body["messages"][0]["content"][0]["text"], "do it");
    assert_eq!(body["tools"][0]["name"], "topodb__search");
    assert_eq!(body["output_format"]["type"], "json_schema");
    assert!(p.headers.iter().any(|(k, _)| k == "anthropic-beta"));
}

#[test]
fn request_maps_tool_result_roundtrip_message() {
    let turn = turn_with(
        vec![ContentPart::ToolResult { tool_use_id: "tu1".into(), content: "found 3".into(), is_error: false }],
        vec![],
        None,
    );
    let p = provider().request(&turn).unwrap();
    let body: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][0]["content"][0]["tool_use_id"], "tu1");
    assert!(body.get("output_format").is_none());
}

#[test]
fn parse_end_turn_text() {
    let body = json!({"content": [{"type":"text","text":"{\"n\":1}"}], "stop_reason":"end_turn"});
    let r = provider().parse(200, body.to_string().as_bytes()).unwrap();
    assert_eq!(r.stop, StopReason::EndTurn);
    assert_eq!(r.parts, vec![ContentPart::Text { text: "{\"n\":1}".into() }]);
}

#[test]
fn parse_tool_use() {
    let body = json!({"content": [{"type":"tool_use","id":"tu1","name":"topodb__search","input":{"q":"x"}}], "stop_reason":"tool_use"});
    let r = provider().parse(200, body.to_string().as_bytes()).unwrap();
    assert_eq!(r.stop, StopReason::ToolUse);
    match &r.parts[0] {
        ContentPart::ToolUse { id, name, input } => {
            assert_eq!(id, "tu1");
            assert_eq!(name, "topodb__search");
            assert_eq!(input["q"], "x");
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn parse_api_error_is_malformed_with_message() {
    let body = json!({"type":"error","error":{"type":"invalid_request_error","message":"bad key"}});
    let err = provider().parse(401, body.to_string().as_bytes()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("401") && msg.contains("bad key"), "got: {msg}");
}

#[test]
fn from_env_missing_key_is_config_error() {
    std::env::remove_var("ANTHROPIC_API_KEY");
    let err = AnthropicProvider::from_env(None).unwrap_err();
    assert!(err.to_string().contains("ANTHROPIC_API_KEY"));
}
