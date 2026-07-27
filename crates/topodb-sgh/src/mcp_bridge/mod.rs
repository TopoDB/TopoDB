//! Minimal MCP stdio client: exactly the subset needed to host topodb-mcp
//! for HTTP providers (initialize, tools/list, tools/call). Not a general
//! MCP client. JSON-RPC 2.0, one message per line (MCP stdio framing).
use crate::provider::ToolDef;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const TOOL_NAMESPACE: &str = "topodb__";

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("mcp bridge io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp server sent malformed response: {0}")]
    Malformed(String),
    #[error("mcp server exited or closed its pipe")]
    ServerGone,
    #[error("mcp tool call failed: {0}")]
    Tool(String),
}

#[derive(Debug)]
pub struct McpBridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    tools: Vec<ToolDef>,
}

impl McpBridge {
    /// Spawn the server from pre-validated argv (rails::validate_mcp_server_command),
    /// run the initialize handshake, and list tools. Tool names come back
    /// namespaced `topodb__<name>`.
    pub fn spawn(argv: &[String]) -> Result<Self, BridgeError> {
        if argv.is_empty() {
            return Err(BridgeError::Malformed("empty server command".to_string()));
        }

        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BridgeError::Io(std::io::Error::other("failed to open child stdin")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BridgeError::Io(std::io::Error::other("failed to open child stdout")))?;

        let stdout = BufReader::new(stdout);
        let mut bridge = McpBridge {
            child,
            stdin,
            stdout,
            next_id: 1,
            tools: Vec::new(),
        };

        // Perform initialize handshake
        bridge.initialize()?;

        // Send notifications/initialized notification
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        bridge.send_request(&notification)?;

        // List tools
        bridge.list_tools()?;

        Ok(bridge)
    }

    pub fn tools(&self) -> &[ToolDef] {
        &self.tools
    }

    /// `name` is the namespaced form the model saw; the prefix is stripped
    /// before tools/call. Returns concatenated text content on success;
    /// Err(Tool) carries the server's error content when isError is true.
    pub fn call(&mut self, name: &str, arguments: &Value) -> Result<String, BridgeError> {
        if !name.starts_with(TOOL_NAMESPACE) {
            return Err(BridgeError::Malformed(format!(
                "tool name must start with {}",
                TOOL_NAMESPACE
            )));
        }

        let stripped_name = &name[TOOL_NAMESPACE.len()..];

        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/call",
            "params": {
                "name": stripped_name,
                "arguments": arguments
            }
        });

        let id = self.next_id;
        self.next_id += 1;
        self.send_request(&request)?;
        let response = self.read_response(id)?;

        let result = Self::expect_result(&response)?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut text_parts = Vec::new();
        if let Some(content_array) = result.get("content").and_then(|v| v.as_array()) {
            for item in content_array {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        text_parts.push(text.to_string());
                    }
                }
            }
        }

        let text = text_parts.join("\n");

        if is_error {
            Err(BridgeError::Tool(text))
        } else {
            Ok(text)
        }
    }

    fn initialize(&mut self) -> Result<(), BridgeError> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "sgh",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });

        let id = self.next_id;
        self.next_id += 1;
        self.send_request(&request)?;
        let response = self.read_response(id)?;

        let _ = Self::expect_result(&response)?;
        Ok(())
    }

    fn list_tools(&mut self) -> Result<(), BridgeError> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/list",
            "params": {}
        });

        let id = self.next_id;
        self.next_id += 1;
        self.send_request(&request)?;
        let response = self.read_response(id)?;

        let result = Self::expect_result(&response)?;
        if let Some(tools_array) = result.get("tools").and_then(|v| v.as_array()) {
            for tool_obj in tools_array {
                if let Some(name) = tool_obj.get("name").and_then(|v| v.as_str()) {
                    let description = tool_obj
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let input_schema = tool_obj.get("inputSchema").cloned().unwrap_or(json!({}));

                    self.tools.push(ToolDef {
                        name: format!("{}{}", TOOL_NAMESPACE, name),
                        description,
                        input_schema,
                    });
                } else {
                    return Err(BridgeError::Malformed("tool missing name".to_string()));
                }
            }
            Ok(())
        } else {
            Err(BridgeError::Malformed(
                "tools result missing tools array".to_string(),
            ))
        }
    }

    fn send_request(&mut self, request: &Value) -> Result<(), BridgeError> {
        let line = serde_json::to_string(request)
            .map_err(|e| BridgeError::Malformed(format!("failed to serialize request: {}", e)))?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn expect_result(response: &Value) -> Result<Value, BridgeError> {
        if let Some(result) = response.get("result") {
            Ok(result.clone())
        } else if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            Err(BridgeError::Malformed(message.to_string()))
        } else {
            Err(BridgeError::Malformed(
                "response has no result or error field".to_string(),
            ))
        }
    }

    fn read_response(&mut self, awaited_id: u64) -> Result<Value, BridgeError> {
        loop {
            let mut line = String::new();
            let bytes_read = self.stdout.read_line(&mut line)?;

            if bytes_read == 0 {
                return Err(BridgeError::ServerGone);
            }

            let json: Value = serde_json::from_str(&line)
                .map_err(|e| BridgeError::Malformed(format!("invalid json: {}", e)))?;

            // Check if this is a notification (method but no id)
            if json.get("method").is_some() && json.get("id").is_none() {
                // Skip notifications
                continue;
            }

            // Check if this is a server-to-client request (method and id)
            if json.get("method").is_some() && json.get("id").is_some() {
                // Reply with method not found
                if let Some(id) = json.get("id") {
                    let error_response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": "method not found"
                        }
                    });
                    let _ = self.send_request(&error_response);
                }
                continue;
            }

            // This should be a response (has id)
            if let Some(id_val) = json.get("id") {
                let response_id = id_val.as_u64().ok_or_else(|| {
                    BridgeError::Malformed("response id is not a number".to_string())
                })?;

                if response_id != awaited_id {
                    return Err(BridgeError::Malformed(format!(
                        "response id mismatch: expected {}, got {}",
                        awaited_id, response_id
                    )));
                }

                return Ok(json);
            }

            // No id and no method - malformed
            return Err(BridgeError::Malformed("response has no id".to_string()));
        }
    }
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
