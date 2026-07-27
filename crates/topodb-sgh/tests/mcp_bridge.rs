//! McpBridge against a scripted fake MCP server. The fake reads one JSON-RPC
//! message per line and answers from a canned table, which pins both the
//! framing (line-delimited) and the subset of MCP sgh depends on.
use serde_json::json;
use topodb_sgh::mcp_bridge::{BridgeError, McpBridge};

/// A fake server: responds to initialize, ignores the initialized
/// notification, lists one `search` tool, echoes tools/call arguments.
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
      case "$line" in
        *'"boom"'*)
          printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"it broke"}],"isError":true}}' ;;
        *)
          printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"content":[{"type":"text","text":"found 2 results"}],"isError":false}}' ;;
      esac ;;
  esac
done
"#;
    vec!["sh".to_string(), "-c".to_string(), script.to_string()]
}

#[test]
fn spawn_lists_namespaced_tools() {
    let bridge = McpBridge::spawn(&fake_server()).unwrap();
    let tools = bridge.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "topodb__search");
    assert_eq!(tools[0].description, "find things");
    assert_eq!(tools[0].input_schema["type"], "object");
}

#[test]
fn call_strips_namespace_and_returns_text() {
    let mut bridge = McpBridge::spawn(&fake_server()).unwrap();
    let out = bridge.call("topodb__search", &json!({"q": "x"})).unwrap();
    assert_eq!(out, "found 2 results");
}

#[test]
fn tool_error_content_becomes_tool_err() {
    let mut bridge = McpBridge::spawn(&fake_server()).unwrap();
    let err = bridge
        .call("topodb__search", &json!({"q": "boom"}))
        .unwrap_err();
    match err {
        BridgeError::Tool(msg) => assert_eq!(msg, "it broke"),
        other => panic!("expected Tool error, got {other:?}"),
    }
}

#[test]
fn dead_server_is_server_gone() {
    let bridge = McpBridge::spawn(&fake_server()).unwrap();
    // `true` exits immediately: respawn a bridge over a dead command.
    drop(bridge);
    let argv = vec!["sh".into(), "-c".into(), "exit 0".into()];
    match McpBridge::spawn(&argv) {
        Err(BridgeError::ServerGone) | Err(BridgeError::Io(_)) => {}
        other => panic!("expected ServerGone/Io, got {other:?}"),
    }
}

#[test]
fn initialize_error_response_is_malformed() {
    // Fake replies to initialize with a JSON-RPC error object.
    let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"unsupported protocol"}}' ;;
  esac
done
"#;
    let argv = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
    match McpBridge::spawn(&argv) {
        Err(BridgeError::Malformed(msg)) => {
            assert!(msg.contains("unsupported protocol"), "got: {msg}")
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}
