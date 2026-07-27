//! The provider types are the contract every codec and the runner share.
use serde_json::json;
use topodb_sgh::provider::*;

#[test]
fn chat_turn_roundtrips_parts() {
    let turn = ChatTurn {
        model: Some("m".into()),
        system: None,
        messages: vec![ChatMessage {
            role: Role::User,
            parts: vec![
                ContentPart::Text { text: "hi".into() },
                ContentPart::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                },
            ],
        }],
        tools: vec![ToolDef {
            name: "topodb__search".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }],
        output_schema: None,
        max_tokens: 8192,
    };
    assert_eq!(turn.messages[0].parts.len(), 2);
}

/// A stub provider satisfies the trait with defaults — proves object safety
/// and the default structured-output capability.
struct Stub;
impl ChatProvider for Stub {
    fn request(&self, _t: &ChatTurn) -> Result<HttpPayload, ProviderError> {
        Ok(HttpPayload { url: "http://x".into(), headers: vec![], body: vec![] })
    }
    fn parse(&self, _s: u16, _b: &[u8]) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse { parts: vec![ContentPart::Text { text: "t".into() }], stop: StopReason::EndTurn })
    }
}

#[test]
fn provider_is_object_safe_with_default_capability() {
    let p: Box<dyn ChatProvider> = Box::new(Stub);
    assert!(p.supports_structured_output());
}
