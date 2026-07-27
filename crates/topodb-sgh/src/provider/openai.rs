//! OpenAI-compatible chat/completions codec (works with OpenAI, vLLM, Ollama, etc).
//!
//! This module is a pure request/response mapper for OpenAI-compatible endpoints.
//! It handles both the official OpenAI API and any server implementing the same wire format.

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

/// OpenAI-compatible chat/completions provider.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    api_key: Option<String>,
    model_default: Option<String>,
    base_url: String,
}

impl OpenAiProvider {
    /// Create a new provider.
    ///
    /// `base_url` defaults to `https://api.openai.com/v1` if None.
    /// Trailing `/` on supplied base_url is trimmed.
    ///
    /// Key is required when base_url is None or equals the default.
    /// Key is optional for any other base_url (e.g., local servers).
    pub fn new(
        api_key: Option<String>,
        model_default: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let default_base = "https://api.openai.com/v1";
        let resolved_base = base_url
            .map(|url| {
                if url.ends_with('/') {
                    url[..url.len() - 1].to_string()
                } else {
                    url
                }
            })
            .unwrap_or_else(|| default_base.to_string());

        // Check key requirement: required when base_url is None or equals default
        if api_key.is_none() || api_key.as_ref().map(|k| k.is_empty()).unwrap_or(false) {
            if resolved_base == default_base {
                return Err(ProviderError::Config("OPENAI_API_KEY".to_string()));
            }
        }

        Ok(Self {
            api_key: api_key.filter(|k| !k.is_empty()),
            model_default,
            base_url: resolved_base,
        })
    }

    /// Create a provider from environment variables.
    ///
    /// Reads `OPENAI_API_KEY` (empty string counts as absent).
    /// Then delegates to `new`.
    pub fn from_env(
        model: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.is_empty());
        Self::new(api_key, model, base_url)
    }
}

impl ChatProvider for OpenAiProvider {
    fn request(&self, turn: &ChatTurn) -> Result<HttpPayload, ProviderError> {
        // Resolve model: turn.model -> default -> error
        let model = turn
            .model
            .as_ref()
            .or(self.model_default.as_ref())
            .ok_or_else(|| ProviderError::Config("no model specified".to_string()))?
            .clone();

        // Build messages array - single pass preserving conversation order
        let mut messages = Vec::new();

        // Add system message if present
        if let Some(system) = &turn.system {
            messages.push(json!({
                "role": "system",
                "content": system
            }));
        }

        // Single pass through messages preserving conversation order
        for msg in &turn.messages {
            match msg.role {
                Role::User => {
                    // User role: collect text parts and join with \n\n
                    let text_parts: Vec<String> = msg
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect();

                    if !text_parts.is_empty() {
                        messages.push(json!({
                            "role": "user",
                            "content": text_parts.join("\n\n")
                        }));
                    }

                    // Handle tool result parts - emit as tool messages at this position
                    for part in &msg.parts {
                        if let ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            is_error: _,
                        } = part
                        {
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content
                            }));
                        }
                    }
                }
                Role::Assistant => {
                    // Assistant role: handle text parts and tool_calls separately
                    let text_parts: Vec<String> = msg
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect();

                    let tool_uses: Vec<_> = msg
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::ToolUse { id, name, input } => {
                                Some((id.clone(), name.clone(), input.clone()))
                            }
                            _ => None,
                        })
                        .collect();

                    let mut assistant_msg = if !text_parts.is_empty() {
                        json!({
                            "role": "assistant",
                            "content": text_parts.join("\n\n")
                        })
                    } else {
                        json!({
                            "role": "assistant",
                            "content": Value::Null
                        })
                    };

                    if !tool_uses.is_empty() {
                        let tool_calls: Vec<Value> = tool_uses
                            .into_iter()
                            .map(|(id, name, input)| {
                                json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string()  // JSON-encoded string
                                    }
                                })
                            })
                            .collect();
                        assistant_msg["tool_calls"] = json!(tool_calls);
                    }

                    messages.push(assistant_msg);
                }
            }
        }

        // Build tools array (if any)
        let tools = if turn.tools.is_empty() {
            None
        } else {
            Some(
                turn.tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema
                            }
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

        // Add tools if present
        if let Some(tools) = tools {
            body["tools"] = json!(tools);
        }

        // Add response_format if output_schema present
        if let Some(schema) = &turn.output_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "node_output",
                    "schema": schema
                }
            });
        }

        // Build headers
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];

        // Add authorization header if key present
        if let Some(key) = &self.api_key {
            headers.push(("authorization".to_string(), format!("Bearer {}", key)));
        }

        Ok(HttpPayload {
            url: format!("{}/chat/completions", self.base_url),
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

        // Extract choices array and get first choice
        let choice = val
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .ok_or_else(|| ProviderError::Malformed("missing choices array or empty".to_string()))?;

        // Extract message
        let message = choice
            .get("message")
            .ok_or_else(|| ProviderError::Malformed("missing message field".to_string()))?;

        // Extract finish_reason
        let finish_reason_str = choice
            .get("finish_reason")
            .and_then(|s| s.as_str())
            .ok_or_else(|| ProviderError::Malformed("missing finish_reason".to_string()))?;

        let stop = match finish_reason_str {
            "stop" => StopReason::EndTurn,
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::Other,
        };

        // Map content parts
        let mut parts = Vec::new();

        // Handle content field (text part)
        if let Some(content) = message.get("content") {
            if !content.is_null() {
                if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        parts.push(ContentPart::Text { text: text.to_string() });
                    }
                }
            }
        }

        // Handle tool_calls field
        if let Some(tool_calls) = message.get("tool_calls") {
            if let Some(calls_arr) = tool_calls.as_array() {
                for call in calls_arr {
                    let id = call
                        .get("id")
                        .and_then(|i| i.as_str())
                        .ok_or_else(|| ProviderError::Malformed("tool_call missing id".to_string()))?
                        .to_string();

                    let function = call
                        .get("function")
                        .ok_or_else(|| ProviderError::Malformed("tool_call missing function".to_string()))?;

                    let name = function
                        .get("name")
                        .and_then(|n| n.as_str())
                        .ok_or_else(|| ProviderError::Malformed("function missing name".to_string()))?
                        .to_string();

                    let arguments_str = function
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .ok_or_else(|| ProviderError::Malformed(format!("tool_call {} arguments missing", id)))?;

                    // Parse the JSON string to a Value
                    let input: Value = serde_json::from_str(arguments_str)
                        .map_err(|_| {
                            ProviderError::Malformed(format!(
                                "tool_call {} arguments not valid JSON",
                                id
                            ))
                        })?;

                    parts.push(ContentPart::ToolUse { id, name, input });
                }
            }
        }

        Ok(ChatResponse { parts, stop })
    }
}
