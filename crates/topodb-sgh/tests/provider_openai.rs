#![cfg(feature = "openai")]
use serde_json::{json, Value};
use topodb_sgh::provider::openai::OpenAiProvider;
use topodb_sgh::provider::*;

fn provider() -> OpenAiProvider {
    OpenAiProvider::new(Some("k-test".into()), Some("gpt-x".into()), None).unwrap()
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
fn request_maps_messages_tools_and_response_format() {
    let turn = turn_with(
        vec![ContentPart::Text { text: "do it".into() }],
        vec![ToolDef { name: "topodb__search".into(), description: "d".into(), input_schema: json!({"type":"object"}) }],
        Some(json!({"type":"object","required":["n"]})),
    );
    let p = provider().request(&turn).unwrap();
    assert_eq!(p.url, "https://api.openai.com/v1/chat/completions");
    assert!(p.headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer k-test"));
    let body: Value = serde_json::from_slice(&p.body).unwrap();
    assert_eq!(body["model"], "gpt-x");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "do it");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "topodb__search");
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["schema"]["required"][0], "n");
}

#[test]
fn request_maps_tool_result_to_tool_role_message() {
    let turn = turn_with(
        vec![ContentPart::ToolResult { tool_use_id: "tu1".into(), content: "found 3".into(), is_error: false }],
        vec![],
        None,
    );
    let p = provider().request(&turn).unwrap();
    let body: Value = serde_json::from_slice(&p.body).unwrap();
    // system message first, then the tool-role message
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][1]["tool_call_id"], "tu1");
    assert_eq!(body["messages"][1]["content"], "found 3");
    assert!(body.get("response_format").is_none());
}

#[test]
fn request_allows_keyless_local_base_url() {
    let p = OpenAiProvider::new(None, Some("m".into()), Some("http://localhost:11434/v1".into())).unwrap();
    let payload = p.request(&turn_with(vec![ContentPart::Text { text: "x".into() }], vec![], None)).unwrap();
    assert_eq!(payload.url, "http://localhost:11434/v1/chat/completions");
    assert!(!payload.headers.iter().any(|(k, _)| k == "authorization"));
}

#[test]
fn new_keyless_default_base_is_config_error() {
    let err = OpenAiProvider::new(None, Some("m".into()), None).unwrap_err();
    assert!(err.to_string().contains("OPENAI_API_KEY"), "got: {err}");
}

#[test]
fn parse_stop_with_content() {
    let body = json!({"choices":[{"message":{"content":"{\"n\":1}"},"finish_reason":"stop"}]});
    let r = provider().parse(200, body.to_string().as_bytes()).unwrap();
    assert_eq!(r.stop, StopReason::EndTurn);
    assert_eq!(r.parts, vec![ContentPart::Text { text: "{\"n\":1}".into() }]);
}

#[test]
fn parse_tool_calls_decodes_string_arguments() {
    let body = json!({"choices":[{"message":{"content":null,"tool_calls":[
        {"id":"tu1","type":"function","function":{"name":"topodb__search","arguments":"{\"q\":\"x\"}"}}
    ]},"finish_reason":"tool_calls"}]});
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
fn parse_length_is_max_tokens() {
    let body = json!({"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]});
    let r = provider().parse(200, body.to_string().as_bytes()).unwrap();
    assert_eq!(r.stop, StopReason::MaxTokens);
}

#[test]
fn parse_api_error_carries_status_and_message() {
    let body = json!({"error":{"message":"invalid api key","type":"invalid_request_error"}});
    let err = provider().parse(401, body.to_string().as_bytes()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("401") && msg.contains("invalid api key"), "got: {msg}");
}
