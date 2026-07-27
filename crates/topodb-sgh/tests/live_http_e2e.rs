#![cfg(feature = "anthropic")]

//! Live: real Anthropic API + real topodb-mcp, driven through the HTTP
//! provider path (`HttpChatRunner` + `AnthropicProvider`), proving the same
//! end-to-end MCP wiring tests/live_e2e.rs proves for the claude-code path.
//! Everything else in this crate is mocked; this and live_e2e.rs are the
//! only two tests that touch the network / a real model.
//!
//! Deliberately ONE agent node, no in-graph verification. In the
//! claude-code path (live_e2e.rs) each `claude -p` invocation spawns its own
//! MCP server subprocess and exits, so a downstream command node can safely
//! re-open the db mid-run. On the HTTP path the `McpBridge` is run-scoped:
//! it spawns `topodb-mcp` once for the whole `Executor::run`, and that
//! process holds redb's EXCLUSIVE file lock on the memory db for as long as
//! it's alive. A `command` node in the same graph that shells out to
//! `topodb ... search` while the bridge is still running can only ever fail
//! with `TopoError::Busy` — this was tried and confirmed empirically (a
//! stub-provider run against an identical 3-node shape with an in-graph
//! `verify` command blocked at that step with exactly that error). So there
//! is no `verify` node and no `output.schema` here either (a schema would
//! require a downstream command node to check it — `schema::validate`
//! enforces that pairing, and there's nothing downstream of a single-node
//! graph). The honest check is post-run: drop the runner (and with it the
//! bridge, releasing the lock), THEN open the db directly and search it —
//! do not "fix" this back into an in-graph verify shape; it will compile
//! and always fail live.
//!
//! Opt-in double gate: `#[ignore]` (so plain `cargo test` never sees it) AND
//! env-gated self-skip (so even `--ignored` sweeps pass without the setup).
//! Run it with:
//!   SGH_LIVE_HTTP_E2E=1 ANTHROPIC_API_KEY=... cargo test -p topodb-sgh --test live_http_e2e -- --ignored --nocapture
//! Requires: ANTHROPIC_API_KEY; a topodb-mcp binary at $SGH_E2E_MCP_BIN or
//! target/{release,debug}/topodb-mcp.

use topodb_sgh::executor::Executor;
use topodb_sgh::mcp_bridge::McpBridge;
use topodb_sgh::provider::anthropic::AnthropicProvider;
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

    let dir = tempfile::tempdir().unwrap();
    let mem_db = dir.path().join("e2e-memory.redb");
    let sentinel = format!("sgh live http e2e sentinel {}", ulid::Ulid::new());

    // One agent node, opted into MCP, told to store the sentinel. The
    // `remember` tool requires a non-empty `entities` array, hence the
    // explicit instruction to link it to an entity.
    let yaml = format!(
        r#"
version: 1
goal: live http mcp wiring proof
nodes:
  - id: remember
    kind: agent
    prompt: "Store the fact '{sentinel}' as a memory linked to entity 'sgh-e2e', then reply DONE."
    tools: [topodb]
    budget: {{retries: 1, repairs: 0}}
"#
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

    let run_db = topodb::Db::open(dir.path().join("run.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&run_db, "live-http-e2e", &v, 1).unwrap();
    let mut ex = Executor::new(store, v.clone(), &runner);
    let report = ex.run(2).unwrap();

    eprintln!(
        "Run succeeded: {} blocked: {}",
        report.succeeded.join(","),
        report.blocked.join(",")
    );

    assert!(
        report.blocked.is_empty(),
        "live run blocked (permission denial or model failure): {:?} — reasons: {:?}",
        report.blocked,
        report.blocked_reasons
    );
    assert_eq!(report.succeeded, vec!["remember".to_string()]);

    // Drop the executor and runner explicitly BEFORE reopening the db: the
    // runner owns the McpBridge, which owns the topodb-mcp child process,
    // which holds redb's exclusive lock on mem_db for as long as it's
    // alive. Reopening while it's still running would itself be Busy.
    drop(ex);
    drop(runner);

    // The proof: the memory EXISTS in the target db, checked directly (not
    // through the agent's self-report), only after the lock is released —
    // mirrors live_e2e.rs's assertion style.
    let db = topodb::Db::open_stored(&mem_db).expect("mcp server must have created the db");
    let scopes = topodb::ScopeSet::default().with_shared();
    let hits = db.search_text(&scopes, &sentinel, 5).unwrap();
    assert!(
        !hits.is_empty(),
        "sentinel memory not found — agent did not write through the topodb MCP tools"
    );
}
