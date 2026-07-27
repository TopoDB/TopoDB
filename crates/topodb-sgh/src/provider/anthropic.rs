//! Anthropic Messages API codec (structured-outputs-capable).
//!
//! This module is a pure request/response mapper for the Anthropic Messages API.
//! Structured output (output_format + beta header) shape is verified live during Task 10's e2e tests;
//! if the API rejects it, the e2e task fixes this codec, not the schema itself.

use super::{ChatProvider, ChatResponse, ChatTurn, ContentPart, HttpPayload, ProviderError, Role, StopReason};
use serde_json::{json, Value};

/// Safely elide a response body to ~200 chars, preserving UTF-8 boundaries.
fn elide_body(body: &[u8]) -> String {
    let s = String::from_utf8_lossy(body);
    if s.len() > 200 {
        s.chars().take(200).collect()
    } else {
        s.into_owned()
    }
}

/// Anthropic Messages API provider.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    api_key: String,
    model_default: Option<String>,
    base_url: String,
}

impl AnthropicProvider {
    /// Create a new provider with default base_url `https://api.anthropic.com`.
    pub fn new(api_key: String, model_default: Option<String>) -> Self {
        Self {
            api_key,
            model_default,
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    /// Override the base URL (for tests/custom endpoints).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Create a provider from environment variables.
    /// Reads `ANTHROPIC_API_KEY`; returns `Config` error if absent or empty.
    pub fn from_env(model: Option<String>) -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::Config("ANTHROPIC_API_KEY".to_string()))?;
        if api_key.is_empty() {
            return Err(ProviderError::Config("ANTHROPIC_API_KEY".to_string()));
        }
        Ok(Self::new(api_key, model))
    }
}

impl ChatProvider for AnthropicProvider {
    fn request(&self, turn: &ChatTurn) -> Result<HttpPayload, ProviderError> {
        // Resolve model: turn.model -> default -> error
        let model = turn
            .model
            .as_ref()
            .or(self.model_default.as_ref())
            .ok_or_else(|| ProviderError::Config("no model specified".to_string()))?
            .clone();

        // Build messages array
        let messages = turn
            .messages
            .iter()
            .map(|msg| {
                let role_str = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let content = msg
                    .parts
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => json!({"type": "text", "text": text}),
                        ContentPart::ToolUse { id, name, input } => {
                            json!({"type": "tool_use", "id": id, "name": name, "input": input})
                        }
                        ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": content,
                            "is_error": is_error
                        }),
                    })
                    .collect::<Vec<_>>();
                json!({"role": role_str, "content": content})
            })
            .collect::<Vec<_>>();

        // Build tools array (if any)
        let tools = if turn.tools.is_empty() {
            None
        } else {
            Some(
                turn.tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.input_schema
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        };

        // Build body
        let mut body = json!({
            "model": model,
            "max_tokens": turn.max_tokens,
            "messages": messages,
        });

        // Add system if present
        if let Some(system) = &turn.system {
            body["system"] = json!(system);
        }

        // Add tools if present
        if let Some(tools) = tools {
            body["tools"] = json!(tools);
        }

        // Add output_format if output_schema present
        if let Some(schema) = &turn.output_schema {
            body["output_format"] = json!({
                "type": "json_schema",
                "schema": schema
            });
        }

        // Build headers
        let mut headers = vec![
            ("x-api-key".to_string(), self.api_key.clone()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];

        // Add beta header if output_schema present
        if turn.output_schema.is_some() {
            headers.push((
                "anthropic-beta".to_string(),
                "structured-outputs-2025-11-13".to_string(),
            ));
        }

        Ok(HttpPayload {
            url: format!("{}/v1/messages", self.base_url),
            headers,
            body: serde_json::to_vec(&body)
                .map_err(|_| ProviderError::Malformed("failed to serialize body".to_string()))?,
        })
    }

    fn parse(&self, status: u16, body: &[u8]) -> Result<ChatResponse, ProviderError> {
        // Non-2xx status
        if !(200..300).contains(&status) {
            let parsed: Result<Value, _> = serde_json::from_slice(body);
            let message = if let Ok(val) = parsed {
                val.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| elide_body(body))
            } else {
                elide_body(body)
            };
            return Err(ProviderError::Malformed(format!(
                "HTTP {}: {}",
                status, message
            )));
        }

        // Parse response
        let val: Value = serde_json::from_slice(body)
            .map_err(|_| ProviderError::Malformed("response body not valid JSON".to_string()))?;

        // Extract content array
        let content_array = val
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| ProviderError::Malformed("missing content array".to_string()))?;

        // Extract stop_reason
        let stop_reason_str = val
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ProviderError::Malformed("missing stop_reason".to_string()))?;

        let stop = match stop_reason_str {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            _ => StopReason::Other,
        };

        // Map content parts
        let parts = content_array
            .iter()
            .map(|item| {
                let content_type = item
                    .get("type")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| ProviderError::Malformed("content item missing type".to_string()))?;

                match content_type {
                    "text" => {
                        let text = item
                            .get("text")
                            .and_then(|t| t.as_str())
                            .ok_or_else(|| ProviderError::Malformed("text block missing text field".to_string()))?
                            .to_string();
                        Ok(ContentPart::Text { text })
                    }
                    "tool_use" => {
                        let id = item
                            .get("id")
                            .and_then(|i| i.as_str())
                            .ok_or_else(|| ProviderError::Malformed("tool_use missing id".to_string()))?
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .ok_or_else(|| ProviderError::Malformed("tool_use missing name".to_string()))?
                            .to_string();
                        let input = item
                            .get("input")
                            .ok_or_else(|| ProviderError::Malformed("tool_use missing input".to_string()))?
                            .clone();
                        Ok(ContentPart::ToolUse { id, name, input })
                    }
                    other => Err(ProviderError::Malformed(format!(
                        "unknown content type: {}",
                        other
                    ))),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ChatResponse { parts, stop })
    }
}
