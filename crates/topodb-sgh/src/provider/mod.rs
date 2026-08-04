//! Provider-neutral chat types and the codec seam for HTTP backends.
//!
//! A `ChatProvider` is a pure request/response mapper — it performs no IO.
//! `HttpChatRunner` (runner/http.rs) owns the transport, the tool loop, and
//! every retry bound; a provider only knows how to speak its wire format.
use serde::{Deserialize, Serialize};
use serde_json::Value;

// added in Tasks 5/6
#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "openai")]
pub mod openai;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One tool the model may call, in provider-neutral form (JSON Schema input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

/// Everything a provider needs to build one HTTP call.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatTurn {
    pub model: Option<String>,
    pub system: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    /// When set AND `supports_structured_output()`, the provider must request
    /// native structured output for this JSON Schema.
    pub output_schema: Option<Value>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpPayload {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub parts: Vec<ContentPart>,
    pub stop: StopReason,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider configuration error: {0}")]
    Config(String),
    #[error("provider returned a malformed response: {0}")]
    Malformed(String),
}

/// Pure codec: build the wire payload for one turn; parse one wire response.
/// No IO, no retries, no clocks — those live in `HttpChatRunner`.
pub trait ChatProvider: Send + Sync {
    fn request(&self, turn: &ChatTurn) -> Result<HttpPayload, ProviderError>;
    fn parse(&self, status: u16, body: &[u8]) -> Result<ChatResponse, ProviderError>;
    /// Whether `ChatTurn.output_schema` can be requested natively. When
    /// false, the runner falls back to prose "reply with bare JSON"
    /// instructions plus `extract_json`.
    fn supports_structured_output(&self) -> bool {
        true
    }
}

/// The IO seam under `HttpChatRunner`, injectable for tests.
#[cfg(feature = "http")]
pub trait HttpTransport: Send + Sync {
    /// POST the payload; return (status, body). `Err` is transport-level
    /// (DNS, connect, timeout) — HTTP error statuses are Ok((status, body)).
    fn post(
        &self,
        payload: &HttpPayload,
        timeout: std::time::Duration,
    ) -> Result<(u16, Vec<u8>), std::io::Error>;
}

#[cfg(feature = "http")]
pub struct UreqTransport;

#[cfg(feature = "http")]
impl HttpTransport for UreqTransport {
    fn post(
        &self,
        payload: &HttpPayload,
        timeout: std::time::Duration,
    ) -> Result<(u16, Vec<u8>), std::io::Error> {
        // http_status_as_error(false): 4xx/5xx come back as Ok responses
        // so their bodies stay readable — the HttpTransport contract that
        // send_with_retries and the codecs depend on. The timeout differs
        // per call (remaining-deadline capped), hence per-request config
        // rather than one fixed Agent.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .build();
        let agent: ureq::Agent = config.into();
        let mut req = agent.post(&payload.url);
        for (k, v) in &payload.headers {
            req = req.header(k, v);
        }
        match req.send(&payload.body[..]) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let mut buf = Vec::new();
                use std::io::Read;
                // into_reader() is unbounded by default (the 10MB cap only
                // applies to Body::read_to_vec/read_to_string/read_json), so
                // this mirrors v2's unbounded read_to_end exactly.
                resp.into_body().into_reader().read_to_end(&mut buf)?;
                Ok((status, buf))
            }
            // One contract delta vs the old ureq 2 impl: 3xx responses ureq
            // won't follow (307/308 with a POST body, or any 3xx without a
            // Location header) surface here as Err instead of the old
            // Ok((3xx, body)) — pathological for the fixed API endpoints
            // this transport talks to, but a future reader tracing a
            // "transport failure" on a redirecting endpoint should look here.
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }
}
