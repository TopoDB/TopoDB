//! One AgentRunner for every HTTP chat provider. Owns the tool loop, the
//! structured-output fallback, transport retries, and denial mapping —
//! a ChatProvider is only a wire-format codec.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::mcp_bridge::{BridgeError, McpBridge};
use crate::provider::{
    ChatMessage, ChatProvider, ChatResponse, ChatTurn, ContentPart, HttpPayload, HttpTransport,
    Role, StopReason, ToolDef, UreqTransport,
};
use crate::schema::TOPODB_TOOL;

use super::{cancel::CancelToken, common, AgentRunner, NodeOutcome, NodeRequest, RunnerError};

/// Safely elide a response body to ~200 chars for an error message,
/// preserving UTF-8 boundaries.
fn elide_body(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    if s.chars().count() > 200 {
        s.chars().take(200).collect()
    } else {
        s.into_owned()
    }
}

/// Send one HTTP payload, retrying transport `Err(io)` and 429/5xx statuses
/// up to `1 + retries` total attempts with capped exponential backoff
/// (`backoff_base * 2^attempt`, capped at 30s; a zero `backoff_base` sleeps
/// not at all — tests use this to run instantly).
///
/// Non-retryable statuses (2xx, or 4xx other than 429) return immediately
/// as `Ok((status, body))` — the caller (a `ChatProvider::parse`) decides
/// whether that status is itself an error. Only retry exhaustion and
/// transport-level IO errors are surfaced here as `Err(String)`.
///
/// Shared by `HttpChatRunner` (runner/http.rs) and `ApiBackend`
/// (planner/api.rs) so both providers' bounded retry policy stays identical
/// by construction rather than by two hand-kept copies.
pub(crate) fn send_with_retries(
    transport: &dyn HttpTransport,
    payload: &HttpPayload,
    timeout: Duration,
    retries: u32,
    backoff_base: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let attempts = 1 + retries;
    let mut last_err = String::new();

    for attempt in 0..attempts {
        match transport.post(payload, timeout) {
            Ok((status, body)) => {
                if status == 429 || (500..600).contains(&status) {
                    last_err = format!("status {status}: {}", elide_body(&body));
                    if attempt + 1 < attempts {
                        sleep_backoff(attempt, backoff_base);
                        continue;
                    }
                    return Err(format!(
                        "provider transport failed after {attempts} attempts: {last_err}"
                    ));
                }
                return Ok((status, body));
            }
            Err(io_err) => {
                last_err = io_err.to_string();
                if attempt + 1 < attempts {
                    sleep_backoff(attempt, backoff_base);
                    continue;
                }
                return Err(format!(
                    "provider transport failed after {attempts} attempts: {last_err}"
                ));
            }
        }
    }

    // Unreachable when attempts >= 1 (retries is u32, so attempts >= 1
    // always), kept for exhaustiveness.
    Err(format!(
        "provider transport failed after {attempts} attempts: {last_err}"
    ))
}

fn sleep_backoff(attempt: u32, backoff_base: Duration) {
    if backoff_base.is_zero() {
        return;
    }
    let mult = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let sleep = backoff_base.saturating_mul(mult);
    let cap = Duration::from_secs(30);
    std::thread::sleep(sleep.min(cap));
}

pub struct HttpChatRunner {
    provider: Box<dyn ChatProvider>,
    transport: Box<dyn HttpTransport>,
    model: Option<String>,
    bridge: Option<Mutex<McpBridge>>,
    /// Upper bound on model turns per node execution. The CLI backend's
    /// internal agent loop is opaque and bounded by claude itself; this is
    /// the HTTP analogue, bounded here. One node execution still counts as
    /// ONE model call in Bound/RunReport terms regardless of rounds.
    pub max_tool_rounds: u32,
    pub max_tokens: u32,
    pub request_timeout: Duration,
    pub max_transport_retries: u32,
    /// Base for exponential backoff between transport retries
    /// (attempt n sleeps `backoff_base * 2^n`, capped at 30s).
    /// Default 1s; tests set Duration::ZERO.
    pub backoff_base: Duration,
    /// Whole-node wall-clock budget (default 600s). Checked before every
    /// transport send and every bridge call; the per-request timeout used
    /// for a send is `remaining.min(request_timeout)`, so a large
    /// `request_timeout` never lets one send outlive the node deadline.
    ///
    /// Limitation: a hung bridge call still hangs regardless of this
    /// deadline — MCP bridge calls read from a pipe with no timeout of
    /// their own, so this field cannot bound them, only the transport
    /// sends around them. Phase 3's events make a stuck bridge call
    /// observable from the outside even though this field cannot kill it.
    pub node_deadline: Duration,
    /// Cooperative cancellation token. When set and cancelled, the runner
    /// aborts inflight work and returns a failed outcome.
    pub cancel: Option<CancelToken>,
}

impl HttpChatRunner {
    pub fn new(
        provider: Box<dyn ChatProvider>,
        model: Option<String>,
        bridge: Option<McpBridge>,
    ) -> Self {
        Self::with_transport(provider, Box::new(UreqTransport), model, bridge)
    }

    pub fn with_transport(
        provider: Box<dyn ChatProvider>,
        transport: Box<dyn HttpTransport>,
        model: Option<String>,
        bridge: Option<McpBridge>,
    ) -> Self {
        HttpChatRunner {
            provider,
            transport,
            model,
            bridge: bridge.map(Mutex::new),
            max_tool_rounds: 16,
            max_tokens: 8192,
            request_timeout: Duration::from_secs(600),
            max_transport_retries: 3,
            backoff_base: Duration::from_secs(1),
            node_deadline: Duration::from_secs(600),
            cancel: None,
        }
    }

    /// Send one turn, retrying transport `Err(io)` and 429/5xx statuses up to
    /// `1 + max_transport_retries` total attempts with capped exponential
    /// backoff (delegated to the free function `send_with_retries`, shared
    /// with `ApiBackend` so both providers' retry policy is identical by
    /// construction). Every failure path returns the provider's parse error
    /// text (built by calling `provider.parse` on the final response) rather
    /// than a raw status code, so the caller doesn't need its own mapping.
    ///
    /// Simplification (Phase 1): the transport does not surface response
    /// headers, so a numeric `retry-after` cannot be honored even though the
    /// spec allows it — we always use the computed exponential backoff.
    fn send_with_retries(
        &self,
        turn: &ChatTurn,
        timeout: Duration,
    ) -> Result<ChatResponse, String> {
        let payload = match self.provider.request(turn) {
            Ok(p) => p,
            Err(e) => return Err(e.to_string()),
        };

        let (status, body) = send_with_retries(
            self.transport.as_ref(),
            &payload,
            timeout,
            self.max_transport_retries,
            self.backoff_base,
        )?;

        match self.provider.parse(status, &body) {
            Ok(resp) => Ok(resp),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl AgentRunner for HttpChatRunner {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        // `Instant + Duration` panics on overflow; an operator-supplied
        // `--agent-timeout` near `u64::MAX` seconds could otherwise take
        // down the run instead of just being a very long (effectively
        // unbounded) deadline. Clamp to a far-future-but-safe ceiling first.
        let safe_deadline = self.node_deadline.min(Duration::from_secs(86400 * 365));
        let deadline_at = Instant::now() + safe_deadline;
        // Returns `Some(remaining)` while there's still budget, or emits the
        // Failed outcome and returns None once the deadline has passed.
        // Called before every transport send and every bridge call so no
        // path can start new work once the whole-node budget is spent.
        macro_rules! remaining_or_fail {
            () => {{
                if let Some(token) = &self.cancel {
                    if token.is_cancelled() {
                        return Ok(NodeOutcome::Failed {
                            error: "cancelled".into(),
                        });
                    }
                }
                let remaining = deadline_at.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(NodeOutcome::Failed {
                        error: format!("deadline exceeded after {}s", self.node_deadline.as_secs()),
                    });
                }
                remaining
            }};
        }

        // Clause 1: tool surface.
        let wants_topodb = req.tools.iter().any(|t| t == TOPODB_TOOL);
        let tools: Vec<ToolDef> = if wants_topodb {
            match &self.bridge {
                None => {
                    return Ok(NodeOutcome::Failed {
                        error: "node declares tools: [topodb] but the run supplied no --agent-mcp server".to_string(),
                    });
                }
                Some(bridge) => {
                    remaining_or_fail!();
                    match bridge.lock() {
                        Ok(guard) => guard.tools().to_vec(),
                        Err(_) => {
                            return Ok(NodeOutcome::Failed {
                                error: "mcp bridge mutex was poisoned".to_string(),
                            });
                        }
                    }
                }
            }
        } else {
            Vec::new()
        };

        // Clause 2: structured output, native vs. fallback.
        let native = req.output_schema.is_some() && self.provider.supports_structured_output();
        let (turn_output_schema, prompt_req);
        if native {
            turn_output_schema = req.output_schema.clone();
            let mut r = req.clone();
            r.output_schema = None;
            prompt_req = r;
        } else {
            turn_output_schema = None;
            prompt_req = req.clone();
        }
        let base_prompt = common::build_prompt(&prompt_req);

        let mut messages = vec![ChatMessage {
            role: Role::User,
            parts: vec![ContentPart::Text { text: base_prompt }],
        }];

        // Clause 3/4: bounded tool loop.
        for round in 0..self.max_tool_rounds {
            let turn = ChatTurn {
                model: self.model.clone(),
                system: None,
                messages: messages.clone(),
                tools: tools.clone(),
                output_schema: turn_output_schema.clone(),
                max_tokens: self.max_tokens,
            };

            let remaining = remaining_or_fail!();
            let timeout = remaining.min(self.request_timeout);
            let response = match self.send_with_retries(&turn, timeout) {
                Ok(r) => r,
                Err(e) => return Ok(NodeOutcome::Failed { error: e }),
            };

            let tool_uses: Vec<(&String, &String, &serde_json::Value)> = response
                .parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolUse { id, name, input } => Some((id, name, input)),
                    _ => None,
                })
                .collect();

            if !tool_uses.is_empty() {
                for (_, name, _) in &tool_uses {
                    if !tools.iter().any(|t| &t.name == *name) {
                        return Ok(NodeOutcome::Denied {
                            tool: (*name).clone(),
                        });
                    }
                }

                // Push the assistant turn verbatim.
                messages.push(ChatMessage {
                    role: Role::Assistant,
                    parts: response.parts.clone(),
                });

                let mut result_parts = Vec::with_capacity(tool_uses.len());
                for (id, name, input) in &tool_uses {
                    remaining_or_fail!();
                    let bridge = self
                        .bridge
                        .as_ref()
                        .expect("tool_uses non-empty implies bridge was validated above");
                    let mut guard = match bridge.lock() {
                        Ok(g) => g,
                        Err(_) => {
                            return Ok(NodeOutcome::Failed {
                                error: "mcp bridge mutex was poisoned".to_string(),
                            });
                        }
                    };
                    match guard.call(name, input) {
                        Ok(text) => result_parts.push(ContentPart::ToolResult {
                            tool_use_id: (*id).clone(),
                            content: text,
                            is_error: false,
                        }),
                        Err(BridgeError::Tool(msg)) => result_parts.push(ContentPart::ToolResult {
                            tool_use_id: (*id).clone(),
                            content: msg,
                            is_error: true,
                        }),
                        Err(e) => {
                            return Ok(NodeOutcome::Failed {
                                error: format!("mcp bridge failed: {e}"),
                            });
                        }
                    }
                }

                messages.push(ChatMessage {
                    role: Role::User,
                    parts: result_parts,
                });

                let _ = round;
                continue;
            }

            // Terminal: no tool calls this round.
            if response.stop == StopReason::MaxTokens {
                return Ok(NodeOutcome::Failed {
                    error: "provider stopped at max_tokens before completing".to_string(),
                });
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
                return Ok(NodeOutcome::Failed {
                    error: "provider returned an empty reply".to_string(),
                });
            }

            let output = if req.output_schema.is_some() && !native {
                common::extract_json(&text).unwrap_or(text)
            } else {
                text
            };

            return Ok(NodeOutcome::Succeeded { output });
        }

        Ok(NodeOutcome::Failed {
            error: format!(
                "tool loop exceeded {} rounds without a final answer",
                self.max_tool_rounds
            ),
        })
    }
}
