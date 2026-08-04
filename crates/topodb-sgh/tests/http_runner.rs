#![cfg(feature = "http")]
//! HttpChatRunner behavior: tool loop, structured-output native/fallback,
//! denial mapping, bounded transport retries. Scripted provider + transport
//! stand in for a real HTTP backend so every clause is deterministic.
//!
//! Both scripted types are Arc-backed and `Clone`: the runner takes
//! ownership of a `Box<dyn ...>`, so tests keep a cloned handle (sharing the
//! same inner state) to inspect recorded turns / call counts afterward.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use topodb_sgh::mcp_bridge::OnDemandBridge;
use topodb_sgh::provider::{
    ChatProvider, ChatResponse, ChatTurn, ContentPart, HttpPayload, HttpTransport, ProviderError,
    StopReason,
};
use topodb_sgh::runner::http::HttpChatRunner;
use topodb_sgh::runner::{AgentRunner, NodeOutcome, NodeRequest};

/// Fake MCP server: same shape as tests/mcp_bridge.rs's fake_server(), lists
/// one `search` tool, echoes tools/call arguments.
fn fake_server() -> Vec<String> {
    let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}' ;;
    *'"notifications/initialized"'*)
      : ;;
    *'"tools/list"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"tools":[{"name":"search","description":"find things","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}]}}' ;;
    *'"tools/call"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"found 2 results"}],"isError":false}}' ;;
  esac
done
"#;
    vec!["sh".to_string(), "-c".to_string(), script.to_string()]
}

/// Records every ChatTurn it's asked to build a payload for and pops
/// scripted responses on parse. Non-2xx statuses produce a Malformed error
/// (mirroring a real codec's status handling) instead of consulting the
/// script, which is what lets the retry/no-retry tests key purely off the
/// transport's scripted statuses.
#[derive(Clone)]
struct ScriptedProvider {
    recorded: Arc<Mutex<Vec<ChatTurn>>>,
    responses: Arc<Mutex<Vec<ChatResponse>>>,
    native: bool,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        ScriptedProvider {
            recorded: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
            native: true,
        }
    }

    fn non_native(responses: Vec<ChatResponse>) -> Self {
        ScriptedProvider {
            recorded: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses)),
            native: false,
        }
    }
}

impl ChatProvider for ScriptedProvider {
    fn request(&self, turn: &ChatTurn) -> Result<HttpPayload, ProviderError> {
        self.recorded.lock().unwrap().push(turn.clone());
        Ok(HttpPayload {
            url: "http://scripted".into(),
            headers: vec![],
            body: vec![],
        })
    }

    fn parse(&self, status: u16, _body: &[u8]) -> Result<ChatResponse, ProviderError> {
        if !(200..300).contains(&status) {
            return Err(ProviderError::Malformed(format!("status {status}")));
        }
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            panic!("ScriptedProvider::parse called more times than scripted");
        }
        Ok(responses.remove(0))
    }

    fn supports_structured_output(&self) -> bool {
        self.native
    }
}

/// Scripted (status, body) pairs consumed in order by `ScriptedTransport`.
type ScriptedResponses = Arc<Mutex<Vec<(u16, Vec<u8>)>>>;

/// Returns scripted (status, body) pairs in order; records call count.
#[derive(Clone)]
struct ScriptedTransport {
    responses: ScriptedResponses,
    calls: Arc<Mutex<u32>>,
}

impl ScriptedTransport {
    fn new(statuses: Vec<u16>) -> Self {
        ScriptedTransport {
            responses: Arc::new(Mutex::new(
                statuses.into_iter().map(|s| (s, Vec::new())).collect(),
            )),
            calls: Arc::new(Mutex::new(0)),
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().unwrap()
    }
}

impl HttpTransport for ScriptedTransport {
    fn post(
        &self,
        _payload: &HttpPayload,
        _timeout: Duration,
    ) -> Result<(u16, Vec<u8>), std::io::Error> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            panic!("ScriptedTransport::post called more times than scripted");
        }
        Ok(responses.remove(0))
    }
}

fn base_req(output_schema: Option<serde_json::Value>, tools: Vec<String>) -> NodeRequest {
    NodeRequest {
        node_id: "n".into(),
        prompt: "do it".into(),
        inputs: BTreeMap::new(),
        output_schema,
        tools,
    }
}

fn end_turn(text: &str) -> ChatResponse {
    ChatResponse {
        parts: vec![ContentPart::Text {
            text: text.to_string(),
        }],
        stop: StopReason::EndTurn,
    }
}

fn runner_with(
    provider: ScriptedProvider,
    transport: ScriptedTransport,
    bridge: Option<OnDemandBridge>,
) -> HttpChatRunner {
    let mut r =
        HttpChatRunner::with_transport(Box::new(provider), Box::new(transport), None, bridge);
    r.request_timeout = Duration::from_secs(1);
    r.backoff_base = Duration::ZERO;
    r
}

#[test]
fn succeeds_on_end_turn_text() {
    let provider = ScriptedProvider::new(vec![end_turn("{\"n\":1}")]);
    let transport = ScriptedTransport::new(vec![200]);
    let runner = runner_with(provider, transport, None);
    let req = base_req(None, vec![]);
    let outcome = runner.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Succeeded {
            output: "{\"n\":1}".into()
        }
    );
}

#[test]
fn schema_native_omits_prompt_schema_section() {
    let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
    let provider = ScriptedProvider::new(vec![end_turn("{\"n\":1}")]);
    let inspect = provider.clone();
    let transport = ScriptedTransport::new(vec![200]);
    let runner = runner_with(provider, transport, None);
    let req = base_req(Some(schema.clone()), vec![]);
    let outcome = runner.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Succeeded {
            output: "{\"n\":1}".into()
        }
    );

    let recorded = inspect.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].output_schema, Some(schema));
    let user_text: String = recorded[0]
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !user_text.contains("## Required output"),
        "native path should omit the prompt schema section: {user_text}"
    );
}

#[test]
fn schema_fallback_extracts_json() {
    let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
    let provider = ScriptedProvider::non_native(vec![end_turn("```json\n{\"n\":1}\n```")]);
    let inspect = provider.clone();
    let transport = ScriptedTransport::new(vec![200]);
    let runner = runner_with(provider, transport, None);
    let req = base_req(Some(schema), vec![]);
    let outcome = runner.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Succeeded {
            output: "{\"n\":1}".into()
        }
    );

    let recorded = inspect.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].output_schema, None);
    let user_text: String = recorded[0]
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        user_text.contains("## Required output"),
        "fallback path should keep the prompt schema section: {user_text}"
    );
}

#[test]
fn tool_loop_executes_bridge_and_feeds_result() {
    let bridge = OnDemandBridge::new(fake_server());
    let provider = ScriptedProvider::new(vec![
        ChatResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t1".into(),
                name: "topodb__search".into(),
                input: json!({"q": "x"}),
            }],
            stop: StopReason::ToolUse,
        },
        end_turn("done"),
    ]);
    let inspect = provider.clone();
    let transport = ScriptedTransport::new(vec![200, 200]);
    let runner = runner_with(provider, transport, Some(bridge));
    let req = base_req(None, vec!["topodb".into()]);
    let outcome = runner.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Succeeded {
            output: "done".into()
        }
    );

    let recorded = inspect.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    let second_turn_has_result = recorded[1].messages.iter().any(|m| {
        m.parts.iter().any(|p| match p {
            ContentPart::ToolResult {
                content,
                tool_use_id,
                is_error,
            } => content == "found 2 results" && tool_use_id == "t1" && !is_error,
            _ => false,
        })
    });
    assert!(
        second_turn_has_result,
        "second turn should carry the bridge's ToolResult"
    );
}

#[test]
fn out_of_surface_tool_is_denied() {
    let bridge = OnDemandBridge::new(fake_server());
    let provider = ScriptedProvider::new(vec![ChatResponse {
        parts: vec![ContentPart::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({}),
        }],
        stop: StopReason::ToolUse,
    }]);
    let transport = ScriptedTransport::new(vec![200]);
    let runner = runner_with(provider, transport, Some(bridge));
    let req = base_req(None, vec!["topodb".into()]);
    let outcome = runner.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Denied {
            tool: "Bash".into()
        }
    );
}

#[test]
fn node_without_optin_gets_no_tools() {
    let bridge = OnDemandBridge::new(fake_server());
    let provider = ScriptedProvider::new(vec![end_turn("ok")]);
    let inspect = provider.clone();
    let transport = ScriptedTransport::new(vec![200]);
    let runner = runner_with(provider, transport, Some(bridge));
    let req = base_req(None, vec![]);
    let outcome = runner.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Succeeded {
            output: "ok".into()
        }
    );

    let recorded = inspect.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].tools.is_empty());
}

#[test]
fn missing_bridge_with_optin_fails() {
    let provider = ScriptedProvider::new(vec![]);
    let transport = ScriptedTransport::new(vec![]);
    let runner = runner_with(provider, transport, None);
    let req = base_req(None, vec!["topodb".into()]);
    let outcome = runner.run(&req).unwrap();
    match outcome {
        NodeOutcome::Failed { error } => assert!(error.contains("--agent-mcp")),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn tool_rounds_cap_fails() {
    let bridge = OnDemandBridge::new(fake_server());
    let responses: Vec<ChatResponse> = (0..2)
        .map(|_| ChatResponse {
            parts: vec![ContentPart::ToolUse {
                id: "t1".into(),
                name: "topodb__search".into(),
                input: json!({"q": "x"}),
            }],
            stop: StopReason::ToolUse,
        })
        .collect();
    let provider = ScriptedProvider::new(responses);
    let transport = ScriptedTransport::new(vec![200, 200]);
    let mut runner = runner_with(provider, transport, Some(bridge));
    runner.max_tool_rounds = 2;
    let req = base_req(None, vec!["topodb".into()]);
    let outcome = runner.run(&req).unwrap();
    match outcome {
        NodeOutcome::Failed { error } => assert!(error.contains("2 rounds"), "error was: {error}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn transport_5xx_retries_then_fails() {
    let provider = ScriptedProvider::new(vec![]);
    let transport = ScriptedTransport::new(vec![500, 500, 500, 500]);
    let inspect = transport.clone();
    let mut runner = runner_with(provider, transport, None);
    runner.max_transport_retries = 3;
    let req = base_req(None, vec![]);
    let outcome = runner.run(&req).unwrap();
    match outcome {
        NodeOutcome::Failed { error } => {
            assert!(error.contains("after 4 attempts"), "error was: {error}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(inspect.calls(), 4);
}

#[test]
fn transport_4xx_no_retry() {
    let provider = ScriptedProvider::new(vec![]);
    let transport = ScriptedTransport::new(vec![401]);
    let inspect = transport.clone();
    let runner = runner_with(provider, transport, None);
    let req = base_req(None, vec![]);
    let outcome = runner.run(&req).unwrap();
    match outcome {
        NodeOutcome::Failed { .. } => {}
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(inspect.calls(), 1);
}

#[test]
fn node_deadline_zero_fails_before_any_transport_call() {
    let provider = ScriptedProvider::new(vec![]);
    let transport = ScriptedTransport::new(vec![]);
    let inspect = transport.clone();
    let mut runner = runner_with(provider, transport, None);
    runner.node_deadline = Duration::ZERO;
    let req = base_req(None, vec![]);
    let outcome = runner.run(&req).unwrap();
    match outcome {
        NodeOutcome::Failed { error } => {
            assert_eq!(error, "deadline exceeded after 0s");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(inspect.calls(), 0);
}

/// Records the `timeout` arg every `post` call receives; always answers a
/// scripted 200 with an empty body.
#[derive(Clone)]
struct SlowTransport {
    seen_timeouts: Arc<Mutex<Vec<Duration>>>,
}

impl SlowTransport {
    fn new() -> Self {
        SlowTransport {
            seen_timeouts: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl HttpTransport for SlowTransport {
    fn post(
        &self,
        _payload: &HttpPayload,
        timeout: Duration,
    ) -> Result<(u16, Vec<u8>), std::io::Error> {
        self.seen_timeouts.lock().unwrap().push(timeout);
        Ok((200, Vec::new()))
    }
}

#[test]
fn per_request_timeout_is_capped_by_remaining_deadline() {
    let provider = ScriptedProvider::new(vec![end_turn("ok")]);
    let transport = SlowTransport::new();
    let inspect = transport.seen_timeouts.clone();
    let mut r = HttpChatRunner::with_transport(Box::new(provider), Box::new(transport), None, None);
    r.request_timeout = Duration::from_secs(600);
    r.node_deadline = Duration::from_secs(5);
    r.backoff_base = Duration::ZERO;
    let req = base_req(None, vec![]);
    let outcome = r.run(&req).unwrap();
    assert_eq!(
        outcome,
        NodeOutcome::Succeeded {
            output: "ok".into()
        }
    );
    let seen = inspect.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0] <= Duration::from_secs(5),
        "timeout was {:?}",
        seen[0]
    );
}

#[test]
fn max_tokens_stop_fails() {
    let provider = ScriptedProvider::new(vec![ChatResponse {
        parts: vec![ContentPart::Text {
            text: "partial".into(),
        }],
        stop: StopReason::MaxTokens,
    }]);
    let transport = ScriptedTransport::new(vec![200]);
    let runner = runner_with(provider, transport, None);
    let req = base_req(None, vec![]);
    let outcome = runner.run(&req).unwrap();
    match outcome {
        NodeOutcome::Failed { error } => {
            assert!(error.contains("max_tokens"), "error was: {error}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}
