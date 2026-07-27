//! One AgentRunner for every HTTP chat provider. Owns the tool loop, the
//! structured-output fallback, transport retries, and denial mapping —
//! a ChatProvider is only a wire-format codec.
use std::sync::Mutex;
use std::time::Duration;

use crate::mcp_bridge::{BridgeError, McpBridge};
use crate::provider::{
    ChatMessage, ChatProvider, ChatResponse, ChatTurn, ContentPart, HttpTransport, Role,
    StopReason, ToolDef, UreqTransport,
};
use crate::schema::TOPODB_TOOL;

use super::{common, AgentRunner, NodeOutcome, NodeRequest, RunnerError};

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
        }
    }

    /// Send one turn, retrying transport `Err(io)` and 429/5xx statuses up to
    /// `1 + max_transport_retries` total attempts with capped exponential
    /// backoff. Non-retryable statuses (4xx other than 429) fail on the
    /// first attempt. Every failure path returns the provider's parse error
    /// text (built by calling `provider.parse` on the final response) rather
    /// than a raw status code, so the caller doesn't need its own mapping.
    ///
    /// Simplification (Phase 1): the transport does not surface response
    /// headers, so a numeric `retry-after` cannot be honored even though the
    /// spec allows it — we always use the computed exponential backoff.
    fn send_with_retries(&self, turn: &ChatTurn) -> Result<ChatResponse, String> {
        let payload = match self.provider.request(turn) {
            Ok(p) => p,
            Err(e) => return Err(e.to_string()),
        };

        let attempts = 1 + self.max_transport_retries;
        let mut last_err = String::new();

        for attempt in 0..attempts {
            match self.transport.post(&payload, self.request_timeout) {
                Ok((status, body)) => {
                    if status == 429 || (500..600).contains(&status) {
                        last_err = match self.provider.parse(status, &body) {
                            Ok(_) => format!("status {status}"),
                            Err(e) => e.to_string(),
                        };
                        if attempt + 1 < attempts {
                            self.sleep_backoff(attempt);
                            continue;
                        }
                        return Err(format!(
                            "provider transport failed after {attempts} attempts: {last_err}"
                        ));
                    }
                    // 2xx or a non-retryable status: parse and return either way.
                    return match self.provider.parse(status, &body) {
                        Ok(resp) => Ok(resp),
                        Err(e) => Err(e.to_string()),
                    };
                }
                Err(io_err) => {
                    last_err = io_err.to_string();
                    if attempt + 1 < attempts {
                        self.sleep_backoff(attempt);
                        continue;
                    }
                    return Err(format!(
                        "provider transport failed after {attempts} attempts: {last_err}"
                    ));
                }
            }
        }

        // Unreachable when attempts >= 1 (max_transport_retries is u32, so
        // attempts >= 1 always), kept for exhaustiveness.
        Err(format!(
            "provider transport failed after {attempts} attempts: {last_err}"
        ))
    }

    fn sleep_backoff(&self, attempt: u32) {
        if self.backoff_base.is_zero() {
            return;
        }
        let mult = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
        let sleep = self.backoff_base.saturating_mul(mult);
        let cap = Duration::from_secs(30);
        std::thread::sleep(sleep.min(cap));
    }
}

impl AgentRunner for HttpChatRunner {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        // Clause 1: tool surface.
        let wants_topodb = req.tools.iter().any(|t| t == TOPODB_TOOL);
        let tools: Vec<ToolDef> = if wants_topodb {
            match &self.bridge {
                None => {
                    return Ok(NodeOutcome::Failed {
                        error: "node declares tools: [topodb] but the run supplied no --agent-mcp server".to_string(),
                    });
                }
                Some(bridge) => match bridge.lock() {
                    Ok(guard) => guard.tools().to_vec(),
                    Err(_) => {
                        return Ok(NodeOutcome::Failed {
                            error: "mcp bridge mutex was poisoned".to_string(),
                        });
                    }
                },
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

            let response = match self.send_with_retries(&turn) {
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
