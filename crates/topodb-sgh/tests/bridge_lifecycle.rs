//! Lifecycle tests for OnDemandBridge against a stub MCP server.
//!
//! harness = false: when re-exec'd with `--stub <state-dir>` this same
//! binary IS the stub server; otherwise it runs the assertions below.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--stub" {
        stub_server(Path::new(&args[2]));
        return;
    }
    // Test driver.
    let dir = tempdir();
    test_lease_spawns_and_release_reaps(&dir.join("t1"));
    test_two_leases_share_one_child(&dir.join("t2"));
    test_release_then_lease_respawns(&dir.join("t3"));
    test_tool_error_keeps_child_alive(&dir.join("t4"));
    test_dead_child_clears_slot_and_respawns(&dir.join("t5"));
    test_spawn_failure_leaves_bridge_reusable(&dir.join("t6"));
    test_runner_releases_between_nodes(&dir.join("t7"));
    println!("bridge_lifecycle: all tests passed");
}

fn tempdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("sgh-bridge-lifecycle-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn stub_argv(state_dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(state_dir).unwrap();
    vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--stub".to_string(),
        state_dir.to_string_lossy().into_owned(),
    ]
}

fn spawn_count(state_dir: &Path) -> usize {
    std::fs::read_to_string(state_dir.join("spawns.log"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// True while the stub child that wrote `alive-<pid>` is still running.
/// The pidfile is removed only on clean EOF exit; after a kill it lingers,
/// so liveness must be checked against the process, not the file alone.
fn stub_alive(state_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(pid) = name.strip_prefix("alive-") {
            if pid_running(pid) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
fn pid_running(pid: &str) -> bool {
    // kill -0: signal 0 probes existence without sending anything.
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn pid_running(pid: &str) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(pid),
        Err(_) => false,
    }
}

fn test_lease_spawns_and_release_reaps(dir: &Path) {
    let bridge = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(stub_argv(dir));
    assert_eq!(spawn_count(dir), 0, "new() must not spawn");
    let lease = bridge.lease().expect("lease spawns the stub");
    assert_eq!(spawn_count(dir), 1);
    assert!(stub_alive(dir), "child alive while leased");
    let tools = lease.tools().expect("tools listed");
    assert!(
        tools.iter().any(|t| t.name == "topodb__ping"),
        "namespaced tool"
    );
    let echoed = lease
        .call("topodb__ping", &json!({"msg": "hello"}))
        .expect("call succeeds");
    assert!(echoed.contains("hello"));
    drop(lease);
    assert!(!stub_alive(dir), "child reaped at zero leases");
    println!("  lease_spawns_and_release_reaps: ok");
}

fn test_two_leases_share_one_child(dir: &Path) {
    let bridge = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(stub_argv(dir));
    let a = bridge.lease().unwrap();
    let b = bridge.lease().unwrap();
    assert_eq!(spawn_count(dir), 1, "second lease shares the child");
    drop(a);
    assert!(stub_alive(dir), "child survives while one lease remains");
    drop(b);
    assert!(!stub_alive(dir), "child reaped when the last lease drops");
    println!("  two_leases_share_one_child: ok");
}

fn test_release_then_lease_respawns(dir: &Path) {
    let bridge = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(stub_argv(dir));
    drop(bridge.lease().unwrap());
    let lease = bridge.lease().expect("respawn after full release");
    assert_eq!(spawn_count(dir), 2, "second burst respawns");
    lease
        .call("topodb__ping", &json!({}))
        .expect("respawned child works");
    drop(lease);
    assert!(!stub_alive(dir));
    println!("  release_then_lease_respawns: ok");
}

fn test_tool_error_keeps_child_alive(dir: &Path) {
    let bridge = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(stub_argv(dir));
    let lease = bridge.lease().unwrap();
    let err = lease.call("topodb__toolerr", &json!({})).unwrap_err();
    assert!(matches!(err, topodb_sgh::mcp_bridge::BridgeError::Tool(_)));
    assert!(stub_alive(dir), "Tool error must not kill the child");
    lease
        .call("topodb__ping", &json!({}))
        .expect("same child still serves");
    assert_eq!(spawn_count(dir), 1, "no respawn after a Tool error");
    drop(lease);
    println!("  tool_error_keeps_child_alive: ok");
}

fn test_dead_child_clears_slot_and_respawns(dir: &Path) {
    let bridge = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(stub_argv(dir));
    let lease = bridge.lease().unwrap();
    let err = lease.call("topodb__die", &json!({})).unwrap_err();
    assert!(
        !matches!(err, topodb_sgh::mcp_bridge::BridgeError::Tool(_)),
        "child death is a transport-class error, got: {err}"
    );
    // Same lease, next call: respawns on demand.
    lease
        .call("topodb__ping", &json!({}))
        .expect("respawn mid-lease");
    assert_eq!(spawn_count(dir), 2);
    drop(lease);
    assert!(!stub_alive(dir));
    println!("  dead_child_clears_slot_and_respawns: ok");
}

fn test_spawn_failure_leaves_bridge_reusable(dir: &Path) {
    // argv[0] that cannot exist: lease() must return Err, and a later
    // lease against a fixed OnDemandBridge is not required — what IS
    // required is that the failure does not wedge the refcount. We prove
    // it by leasing twice against the bad argv: both fail cleanly.
    let bad = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(vec![dir
        .join("no-such-binary")
        .to_string_lossy()
        .into_owned()]);
    assert!(bad.lease().is_err());
    assert!(
        bad.lease().is_err(),
        "second lease after spawn failure also fails cleanly"
    );
    println!("  spawn_failure_leaves_bridge_reusable: ok");
}

fn test_runner_releases_between_nodes(dir: &Path) {
    use std::collections::BTreeMap;
    use std::time::Duration;
    use topodb_sgh::provider::{
        ChatProvider, ChatResponse, ChatTurn, ContentPart, HttpPayload, HttpTransport,
        ProviderError, StopReason,
    };

    // Minimal fake provider: round 1 asks for one topodb__ping tool call,
    // round 2 finishes with text. Request bodies encode the round via the
    // message count; parse is driven by a canned script keyed off it.
    struct FakeProvider;
    impl ChatProvider for FakeProvider {
        fn request(&self, turn: &ChatTurn) -> Result<HttpPayload, ProviderError> {
            Ok(HttpPayload {
                url: "http://localhost:0/fake".to_string(),
                headers: vec![],
                body: vec![turn.messages.len() as u8],
            })
        }
        fn parse(&self, _status: u16, body: &[u8]) -> Result<ChatResponse, ProviderError> {
            if body == [1u8] {
                Ok(ChatResponse {
                    parts: vec![ContentPart::ToolUse {
                        id: "tu1".to_string(),
                        name: "topodb__ping".to_string(),
                        input: json!({}),
                    }],
                    stop: StopReason::ToolUse,
                })
            } else {
                Ok(ChatResponse {
                    parts: vec![ContentPart::Text {
                        text: "done".to_string(),
                    }],
                    stop: StopReason::EndTurn,
                })
            }
        }
    }
    // Transport that echoes the request body back so parse sees the round.
    struct EchoTransport;
    impl HttpTransport for EchoTransport {
        fn post(
            &self,
            payload: &HttpPayload,
            _t: Duration,
        ) -> Result<(u16, Vec<u8>), std::io::Error> {
            Ok((200, payload.body.clone()))
        }
    }

    let bridge = topodb_sgh::mcp_bridge::on_demand::OnDemandBridge::new(stub_argv(dir));
    let runner = topodb_sgh::runner::http::HttpChatRunner::with_transport(
        Box::new(FakeProvider),
        Box::new(EchoTransport),
        Some("m".to_string()),
        Some(bridge),
    );
    use topodb_sgh::runner::{AgentRunner, NodeRequest};
    let req = NodeRequest {
        node_id: "n1".to_string(),
        prompt: "do".to_string(),
        inputs: BTreeMap::new(),
        tools: vec!["topodb".to_string()],
        output_schema: None,
    };
    let out1 = runner.run(&req).expect("node 1 runs");
    assert!(
        matches!(out1, topodb_sgh::runner::NodeOutcome::Succeeded { .. }),
        "{out1:?}"
    );
    assert!(!stub_alive(dir), "child released after the node finished");
    let out2 = runner.run(&req).expect("node 2 runs");
    assert!(matches!(
        out2,
        topodb_sgh::runner::NodeOutcome::Succeeded { .. }
    ));
    assert_eq!(spawn_count(dir), 2, "one spawn per tool-using node burst");
    assert!(!stub_alive(dir));
    println!("  runner_releases_between_nodes: ok");
}

// ---------------- stub server ----------------

fn stub_server(state_dir: &Path) {
    let pid = std::process::id();
    let pidfile = state_dir.join(format!("alive-{pid}"));
    std::fs::write(&pidfile, b"").unwrap();
    {
        use std::fs::OpenOptions;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_dir.join("spawns.log"))
            .unwrap();
        writeln!(f, "{pid}").unwrap();
    }
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            break; // clean EOF exit
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = msg.get("id").cloned();
        let reply = match msg.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"protocolVersion": "2025-06-18", "capabilities": {},
                            "serverInfo": {"name": "stub", "version": "0"}}
            })),
            Some("notifications/initialized") => None,
            Some("tools/list") => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"tools": [
                    {"name": "ping", "description": "echo", "inputSchema": {"type": "object"}},
                    {"name": "die", "description": "exit immediately", "inputSchema": {"type": "object"}},
                    {"name": "toolerr", "description": "return tool error", "inputSchema": {"type": "object"}}
                ]}
            })),
            Some("tools/call") => {
                let tool_name = msg["params"]["name"].as_str().unwrap_or("");
                if tool_name == "die" {
                    std::process::exit(0);
                }
                if tool_name == "toolerr" {
                    Some(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "isError": true,
                            "content": [{"type": "text", "text": "boom"}]
                        }
                    }))
                } else {
                    let echo = msg["params"]["arguments"].to_string();
                    Some(json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": echo}]}
                    }))
                }
            }
            _ => None,
        };
        if let Some(r) = reply {
            writeln!(out, "{r}").unwrap();
            out.flush().unwrap();
        }
    }
    let _ = std::fs::remove_file(&pidfile);
}
