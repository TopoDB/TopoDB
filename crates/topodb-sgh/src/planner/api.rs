//! `PlanBackend` over a `ChatProvider`: the planner's HTTP analogue of
//! `claude_planner` (planner/claude.rs). One turn, no tools, no structured
//! output — the planning prompt built by `build_plan_prompt` is already a
//! complete instruction, so it is sent verbatim as the sole user message.
use std::time::Duration;

use crate::provider::{
    ChatMessage, ChatProvider, ChatTurn, ContentPart, HttpTransport, Role, StopReason,
    UreqTransport,
};
use crate::runner::http::send_with_retries;

use super::{PlanBackend, PlannerError};

/// Default per-request timeout (see `HttpChatRunner`'s equivalent field):
/// a planning call has no tool loop to bound, so the only knobs are the
/// per-request deadline and the transport retry policy. The retry policy
/// stays fixed; the timeout is now an overridable field (`with_timeout`) so
/// `--agent-timeout` can thread through.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_TRANSPORT_RETRIES: u32 = 3;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const MAX_TOKENS: u32 = 16384;

/// A `PlanBackend` that drives one `ChatProvider` turn per `complete` call:
/// no system prompt, no tools, no output schema — just the prompt as a
/// single user message, replied to as plain text.
pub struct ApiBackend {
    provider: Box<dyn ChatProvider>,
    transport: Box<dyn HttpTransport>,
    model: Option<String>,
    request_timeout: Duration,
    max_transport_retries: u32,
    backoff_base: Duration,
    max_tokens: u32,
}

impl ApiBackend {
    /// Real transport (`UreqTransport`) — what the CLI uses.
    pub fn new(provider: Box<dyn ChatProvider>, model: Option<String>) -> Self {
        Self::with_transport(provider, Box::new(UreqTransport), model)
    }

    /// Injectable transport, for tests.
    pub fn with_transport(
        provider: Box<dyn ChatProvider>,
        transport: Box<dyn HttpTransport>,
        model: Option<String>,
    ) -> Self {
        ApiBackend {
            provider,
            transport,
            model,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_transport_retries: MAX_TRANSPORT_RETRIES,
            backoff_base: BACKOFF_BASE,
            max_tokens: MAX_TOKENS,
        }
    }

    /// Override the per-request timeout (wired from `--agent-timeout`).
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d;
        self
    }
}

impl PlanBackend for ApiBackend {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError> {
        let turn = ChatTurn {
            model: self.model.clone(),
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                parts: vec![ContentPart::Text {
                    text: prompt.to_string(),
                }],
            }],
            tools: Vec::new(),
            output_schema: None,
            max_tokens: self.max_tokens,
        };

        let payload = self
            .provider
            .request(&turn)
            .map_err(|e| PlannerError::Runner(e.to_string()))?;

        let (status, body) = send_with_retries(
            self.transport.as_ref(),
            &payload,
            self.request_timeout,
            self.max_transport_retries,
            self.backoff_base,
        )
        .map_err(PlannerError::Runner)?;

        let response = self
            .provider
            .parse(status, &body)
            .map_err(|e| PlannerError::Runner(e.to_string()))?;

        if response.stop == StopReason::MaxTokens {
            return Err(PlannerError::Runner(
                "provider stopped at max_tokens".to_string(),
            ));
        }

        let text: String = response
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        if text.is_empty() {
            return Err(PlannerError::Runner(
                "provider returned an empty reply".to_string(),
            ));
        }

        Ok(text)
    }
}
