#![cfg(feature = "claude-code")]

//! The crate's only live-model test. Everything else is mocked; this one
//! spawns the real `claude` CLI with the real `topodb-mcp` server and proves
//! the MCP wiring end-to-end by asserting on DB STATE, not agent self-report.
//!
//! Opt-in double gate: `#[ignore]` (so plain `cargo test` never sees it) AND
//! env-gated self-skip (so even `--ignored` sweeps pass without the setup).
//! Run it with:
//!   SGH_LIVE_E2E=1 cargo test -p topodb-sgh --test live_e2e -- --ignored --nocapture
//! Requires: authenticated `claude` on PATH; a topodb-mcp binary at
//! $SGH_E2E_MCP_BIN or target/{release,debug}/topodb-mcp.

use topodb_sgh::runner::claude::{ClaudeCodeRunner, McpWiring};

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

fn claude_available() -> bool {
    std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[ignore = "live: spawns real claude + topodb-mcp; set SGH_LIVE_E2E=1"]
fn agent_node_writes_memory_through_mcp() {
    if std::env::var("SGH_LIVE_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: SGH_LIVE_E2E != 1");
        return;
    }
    if !claude_available() {
        eprintln!("skipping: no working `claude` on PATH");
        return;
    }
    let Some(mcp) = mcp_bin() else {
        eprintln!("skipping: no topodb-mcp binary (build it or set SGH_E2E_MCP_BIN)");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let mem_db = dir.path().join("e2e-memory.redb");
    let sentinel = format!("sgh live e2e sentinel {}", ulid::Ulid::new());

    // The graph: one agent node, opted in, told to store the sentinel via
    // MCP and nothing else.
    let yaml = format!(
        r#"
version: 1
goal: live mcp wiring proof
nodes:
  - id: store
    kind: agent
    prompt: "You must call the create_memory tool with this exact content:\n\n{sentinel}\n\nAfter the call succeeds, reply DONE."
    tools: [topodb]
    budget: {{retries: 1, repairs: 0}}
"#
    );
    let g = topodb_sgh::schema::Graph::from_yaml(&yaml).unwrap();
    let v = topodb_sgh::schema::validate::validate(&g).unwrap();

    // mcp-config file, same shape bin/sgh.rs writes.
    let config = dir.path().join("mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "mcpServers": { "topodb": {
                "command": mcp.to_string_lossy(),
                "args": ["--db", mem_db.to_string_lossy(), "--scope", "shared", "--embeddings", "off"],
            }}
        })
        .to_string(),
    )
    .unwrap();

    let runner = ClaudeCodeRunner::new(
        Some("haiku".to_string()),
        vec![],
        false,
        Some(McpWiring {
            config_path: config.to_string_lossy().into_owned(),
        }),
        false,
    );

    // Mirror bin/sgh.rs's Run wiring: store + executor over a run db.
    // (bin imports: `use topodb::Db;` and `use topodb_sgh::store::run::RunStore;`)
    let run_db = topodb::Db::open(dir.path().join("run.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&run_db, "live-e2e", &v, 1).unwrap();
    let mut ex = topodb_sgh::executor::Executor::new(store, v.clone(), &runner);
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
    assert_eq!(report.succeeded, vec!["store".to_string()]);
    // Direct assertion: the run store records an Attempt node per FAILED try,
    // and a permission denial surfaces there as "claude was denied ..." even
    // when the retry then succeeds (budget retries: 1 would mask it in
    // `succeeded` alone). Zero recorded attempts = zero denials, unmasked.
    let attempts = ex.store_ref().attempts("store").unwrap();
    assert!(
        attempts.is_empty(),
        "run succeeded but earlier attempts failed (a denial masked by retry?): {attempts:?}"
    );
    // The proof: the memory EXISTS in the target db. Open it with the engine
    // crate and full-text search the sentinel. A self-report cannot fake this.
    // Must use open_stored to read the persisted index spec (which has a text index);
    // open() with default spec has no text index and returns empty results.
    let db = topodb::Db::open_stored(&mem_db).expect("mcp server must have created the db");
    let scopes = topodb::ScopeSet::default().with_shared();
    let hits = db.search_text(&scopes, &sentinel, 5).unwrap();
    assert!(
        !hits.is_empty(),
        "sentinel memory not found — agent did not write through mcp__topodb"
    );
}
