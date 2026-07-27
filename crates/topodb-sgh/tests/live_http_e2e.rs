#![cfg(feature = "anthropic")]

//! Live: real Anthropic API + real topodb-mcp, driven through the HTTP
//! provider path (`HttpChatRunner` + `AnthropicProvider`), proving the same
//! end-to-end MCP wiring tests/live_e2e.rs proves for the claude-code path.
//! Everything else in this crate is mocked; this and live_e2e.rs are the
//! only two tests that touch the network / a real model.
//!
//! Opt-in double gate: `#[ignore]` (so plain `cargo test` never sees it) AND
//! env-gated self-skip (so even `--ignored` sweeps pass without the setup).
//! Run it with:
//!   SGH_LIVE_HTTP_E2E=1 ANTHROPIC_API_KEY=... cargo test -p topodb-sgh --test live_http_e2e -- --ignored --nocapture
//! Requires: ANTHROPIC_API_KEY; a topodb-mcp binary at $SGH_E2E_MCP_BIN or
//! target/{release,debug}/topodb-mcp; a topodb binary at $SGH_E2E_TOPODB_BIN
//! or target/{release,debug}/topodb (for the `verify` command node).

use topodb_sgh::executor::Executor;
use topodb_sgh::mcp_bridge::McpBridge;
use topodb_sgh::provider::anthropic::AnthropicProvider;
use topodb_sgh::runner::command::ShellCommandRunner;
use topodb_sgh::runner::http::HttpChatRunner;

fn mcp_bin() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("SGH_E2E_MCP_BIN") {
        let p = std::path::PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["release", "debug"] {
        let p = ws.join("target").join(profile).join("topodb-mcp");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn topodb_bin() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("SGH_E2E_TOPODB_BIN") {
        let p = std::path::PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["release", "debug"] {
        let p = ws.join("target").join(profile).join("topodb");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[test]
#[ignore = "live: real Anthropic API; set SGH_LIVE_HTTP_E2E=1"]
fn http_agent_with_topodb_tools_end_to_end() {
    if std::env::var("SGH_LIVE_HTTP_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: SGH_LIVE_HTTP_E2E != 1");
        return;
    }
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("skipping: ANTHROPIC_API_KEY not set");
        return;
    }
    let Some(mcp) = mcp_bin() else {
        eprintln!("skipping: no topodb-mcp binary (build it or set SGH_E2E_MCP_BIN)");
        return;
    };
    let Some(topodb) = topodb_bin() else {
        eprintln!("skipping: no topodb binary (build it or set SGH_E2E_TOPODB_BIN)");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let mem_db = dir.path().join("e2e-memory.redb");
    let sentinel = "phase1 e2e ran";

    // 3-node graph: an agent writes the fact through MCP, a command node
    // independently re-reads the db through the `topodb` CLI (not the
    // agent's self-report), and a second agent recalls the fact through MCP
    // with a native structured-output schema.
    let yaml = format!(
        r#"
version: 1
goal: live http e2e — store, verify via CLI, recall with structured output
nodes:
  - id: remember
    kind: agent
    prompt: "Store the fact '{sentinel}' as a memory, then reply DONE."
    tools: [topodb]
    budget: {{retries: 1, repairs: 0}}
  - id: verify
    kind: command
    needs: [remember]
    run: "'{topodb}' --db '{mem_db}' --scope shared search '{sentinel}' | grep -q '{sentinel}'"
    budget: {{retries: 0, repairs: 0}}
  - id: recall
    kind: agent
    needs: [verify]
    prompt: "Search your memory for a fact about '{sentinel}'. Reply with whether you found it."
    tools: [topodb]
    output:
      schema:
        type: object
        required: [found]
        properties:
          found:
            type: boolean
    budget: {{retries: 1, repairs: 0}}
"#,
        sentinel = sentinel,
        topodb = topodb.display(),
        mem_db = mem_db.display(),
    );

    let g = topodb_sgh::schema::Graph::from_yaml(&yaml).unwrap();
    let v = topodb_sgh::schema::validate::validate(&g).unwrap();

    let mcp_argv = vec![
        mcp.to_string_lossy().into_owned(),
        "--db".to_string(),
        mem_db.to_string_lossy().into_owned(),
        "--scope".to_string(),
        "shared".to_string(),
        "--embeddings".to_string(),
        "off".to_string(),
    ];
    let bridge = McpBridge::spawn(&mcp_argv).unwrap();

    let provider = AnthropicProvider::from_env(Some("claude-haiku-4-5".to_string())).unwrap();
    let runner = HttpChatRunner::new(Box::new(provider), None, Some(bridge));
    let command_runner = ShellCommandRunner::new(std::time::Duration::from_secs(60));

    let run_db = topodb::Db::open(dir.path().join("run.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&run_db, "live-http-e2e", &v, 1).unwrap();
    let mut ex = Executor::new(store, v.clone(), &runner).with_command_runner(&command_runner);
    let report = ex.run(2).unwrap();

    eprintln!(
        "Run succeeded: {} blocked: {}",
        report.succeeded.join(","),
        report.blocked.join(",")
    );

    assert!(
        report.blocked.is_empty(),
        "live run blocked: {:?} — reasons: {:?}",
        report.blocked,
        report.blocked_reasons
    );
    assert_eq!(
        report.succeeded,
        vec![
            "remember".to_string(),
            "verify".to_string(),
            "recall".to_string()
        ],
        "all three nodes must succeed, in dependency order"
    );

    // The proof: the memory EXISTS in the target db, checked directly, not
    // through the agent's self-report — mirrors live_e2e.rs's assertion.
    let db = topodb::Db::open_stored(&mem_db).expect("mcp server must have created the db");
    let scopes = topodb::ScopeSet::default().with_shared();
    let hits = db.search_text(&scopes, sentinel, 5).unwrap();
    assert!(
        !hits.is_empty(),
        "sentinel memory not found — agent did not write through the topodb MCP tools"
    );
}
