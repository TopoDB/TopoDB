#![cfg(feature = "anthropic")]

//! Live: real Anthropic API + real topodb-mcp, driven through the HTTP
//! provider path (`HttpChatRunner` + `AnthropicProvider`), proving the same
//! end-to-end MCP wiring tests/live_e2e.rs proves for the claude-code path.
//! Everything else in this crate is mocked; this and live_e2e.rs are the
//! only two tests that touch the network / a real model.
//!
//! Two-node graph, in-graph verify: `remember` (agent, `tools: [topodb]`)
//! writes a sentinel fact through `mcp__topodb`, then `verify` (command)
//! shells the real `topodb` CLI to search the SAME memory db for it,
//! `grep -q`-ing the sentinel out of the JSON and exiting non-zero if it's
//! absent. This is now the expected shape and the feature's acceptance
//! proof: `mcp_bridge::on_demand::OnDemandBridge` is node-scoped, so the
//! `topodb-mcp` child that holds redb's exclusive lock on the memory db is
//! spawned when `remember` starts and killed+waited (releasing the lock)
//! when `remember`'s bridge lease drops, BEFORE `verify` is scheduled — a
//! `command` node between tool-using agent nodes can always open the db.
//! (Previously the HTTP path ran one `McpBridge` for the whole
//! `Executor::run`, holding the lock the entire time; an in-graph `verify`
//! against that shape reliably failed with `TopoError::Busy`. That
//! constraint no longer holds — do not reintroduce a lock workaround here.)
//!
//! We still keep the post-run direct-open assertion too (after dropping the
//! executor and runner): the in-graph `verify` node proves the db is
//! readable MID-run; the direct read proves the write itself, independent
//! of the agent's self-report. Belt and braces.
//!
//! Opt-in double gate: `#[ignore]` (so plain `cargo test` never sees it) AND
//! env-gated self-skip (so even `--ignored` sweeps pass without the setup).
//! Run it with:
//!   SGH_LIVE_HTTP_E2E=1 ANTHROPIC_API_KEY=... cargo test -p topodb-sgh --test live_http_e2e -- --ignored --nocapture
//! Requires: ANTHROPIC_API_KEY; a topodb-mcp binary at $SGH_E2E_MCP_BIN or
//! target/{release,debug}/topodb-mcp; a topodb (CLI) binary at $TOPODB_CLI
//! or target/{release,debug}/topodb, for the `verify` command node.

use std::time::Duration;

use topodb_sgh::executor::Executor;
use topodb_sgh::mcp_bridge::OnDemandBridge;
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

fn topodb_cli_bin() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("TOPODB_CLI") {
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
    let Some(topodb_cli) = topodb_cli_bin() else {
        eprintln!("skipping: no topodb CLI binary (build it or set TOPODB_CLI)");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let mem_db = dir.path().join("e2e-memory.redb");
    let sentinel = format!("sgh live http e2e sentinel {}", ulid::Ulid::new());

    // Two nodes: `remember` (agent, opted into MCP) stores the sentinel;
    // `verify` (command) shells the real `topodb` CLI to search the SAME
    // memory db for it and exits non-zero if absent — proving the db is
    // readable mid-run, immediately after remember's bridge lease drops.
    // The `remember` tool requires a non-empty `entities` array, hence the
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
  - id: verify
    kind: command
    needs: [remember]
    run: "'{topodb_cli}' --db '{mem_db}' --scope shared search --k 5 '{sentinel}' | grep -qF '{sentinel}'"
    budget: {{retries: 0, repairs: 0}}
"#,
        topodb_cli = topodb_cli.display(),
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
    let bridge = OnDemandBridge::new(mcp_argv);

    let provider = AnthropicProvider::from_env(Some("claude-haiku-4-5".to_string())).unwrap();
    let runner = HttpChatRunner::new(Box::new(provider), None, Some(bridge));
    let commands = ShellCommandRunner::new(Duration::from_secs(30));

    let run_db = topodb::Db::open(dir.path().join("run.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&run_db, "live-http-e2e", &v, 1).unwrap();
    let mut ex = Executor::new(store, v.clone(), &runner).with_command_runner(&commands);
    let report = ex.run(2).unwrap();

    eprintln!(
        "Run succeeded: {} blocked: {}",
        report.succeeded.join(","),
        report.blocked.join(",")
    );

    assert!(
        report.blocked.is_empty(),
        "live run blocked (permission denial, model failure, or the in-graph verify command \
         could not find the sentinel — see the note above about the memory db lock): {:?} — \
         reasons: {:?}",
        report.blocked,
        report.blocked_reasons
    );
    assert_eq!(
        report.succeeded,
        vec!["remember".to_string(), "verify".to_string()]
    );

    // Drop the executor and runner explicitly BEFORE reopening the db: the
    // runner owns the OnDemandBridge, and while a lease is held the
    // topodb-mcp child holds redb's exclusive lock on mem_db. By this point
    // in the run every lease has already been dropped (the `verify` node's
    // in-graph read above is the proof), but we drop here too as a second,
    // independent confirmation via a fresh `Db::open_stored`.
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
