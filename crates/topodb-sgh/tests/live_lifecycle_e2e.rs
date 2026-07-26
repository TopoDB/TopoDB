//! Live proof of the whole Phase D loop: real `claude` (haiku) as the judge,
//! real topodb-mcp for its tools, real CLI sweeps — asserting DB STATE is
//! CONSISTENT WITH THE VERDICTS, not merely that the run reported success.
//! Haiku's judgment on the seeded contents is not asserted (either verdict
//! is legitimate); what is asserted is that whatever it SAID happened, DID.
//!
//! Same double gate as live_e2e.rs:
//!   SGH_LIVE_E2E=1 cargo test -p topodb-sgh --test live_lifecycle_e2e -- --ignored --nocapture

use std::time::Duration;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::claude::{ClaudeCodeRunner, McpWiring};
use topodb_sgh::runner::command::ShellCommandRunner;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

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
    if let Ok(p) = std::env::var("SGH_TEST_TOPODB_BIN") {
        let p = std::path::PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["debug", "release"] {
        let p = ws.join("target").join(profile).join("topodb");
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

fn cli(bin: &std::path::Path, db: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = std::process::Command::new(bin)
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("spawn topodb");
    assert!(
        out.status.success(),
        "topodb {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("topodb prints json")
}

#[test]
#[ignore = "live: spawns real claude + topodb-mcp + topodb CLI; set SGH_LIVE_E2E=1"]
fn lifecycle_graph_actions_match_verdicts_live() {
    if std::env::var("SGH_LIVE_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: SGH_LIVE_E2E != 1");
        return;
    }
    if !claude_available() {
        eprintln!("skipping: no working `claude` on PATH");
        return;
    }
    let (Some(mcp), Some(topodb)) = (mcp_bin(), topodb_bin()) else {
        eprintln!("skipping: need topodb-mcp and topodb binaries (build them or set SGH_E2E_MCP_BIN / SGH_TEST_TOPODB_BIN)");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let mem_db = dir.path().join("lifecycle-memory.redb");

    // Seed: a blatant near-duplicate pair plus an obsolete episodic note —
    // material the judge is LIKELY to act on, though it may legitimately
    // keep everything; the assertions below hold either way.
    let m1 = cli(
        &topodb,
        &mem_db,
        &[
            "remember",
            "--content",
            "The release process publishes crates in dependency order",
            "--entity",
            "release",
        ],
    );
    let m2 = cli(
        &topodb,
        &mem_db,
        &[
            "remember",
            "--content",
            "Crates are published in dependency order during the release process",
            "--entity",
            "release",
        ],
    );
    let m3 = cli(&topodb, &mem_db, &["remember", "--content", "Obsolete note: the 2019 staging outage was resolved the same afternoon; nothing remains to do", "--entity", "ops", "--kind", "episodic"]);
    let seeded: Vec<String> = [&m1, &m2, &m3]
        .iter()
        .map(|v| v["memory_id"].as_str().unwrap().to_string())
        .collect();

    let graph_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/sgh/graphs/lifecycle.yaml");
    let g = Graph::from_yaml(&std::fs::read_to_string(&graph_path).unwrap()).unwrap();
    let v = validate(&g).unwrap();

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

    std::env::set_var("SGH_TOPODB", &topodb);
    std::env::set_var("SGH_MEMORY_DB", &mem_db);

    let agent = ClaudeCodeRunner::new(
        Some("haiku".to_string()),
        // no bash grants — production parity: /sgh:lifecycle passes no --agent-bash
        vec![],
        Some(McpWiring {
            config_path: config.to_string_lossy().into_owned(),
        }),
    );
    let commands = ShellCommandRunner::new(Duration::from_secs(120));
    let run_db = topodb::Db::open(dir.path().join("run.redb")).unwrap();
    let store = RunStore::create(&run_db, "live-lifecycle", &v, 1).unwrap();
    let mut ex = Executor::new(store, v, &agent).with_command_runner(&commands);
    let report = ex.run(2).unwrap();

    // Print judge output early for debugging before assertions
    if let Ok(Some(judge_output)) = ex.store_ref().output("judge") {
        eprintln!("judge output: {}", judge_output);
    }

    eprintln!(
        "succeeded: {:?} blocked: {:?} reasons: {:?}",
        report.succeeded, report.blocked, report.blocked_reasons
    );
    assert!(
        report.blocked.is_empty(),
        "live lifecycle run blocked: {:?}",
        report.blocked_reasons
    );
    assert_eq!(report.succeeded, vec!["sweep", "judge", "verify"]);

    // Consistency: every verdict the judge REPORTED is reflected in the db.
    let judge: serde_json::Value = serde_json::from_str(
        &ex.store_ref()
            .output("judge")
            .unwrap()
            .expect("judge output stored"),
    )
    .unwrap();
    eprintln!("verdicts: {judge}");
    let db = topodb::Db::open_stored(&mem_db).unwrap();
    let scopes = topodb::ScopeSet::default().with_shared();
    let live = |id: &str| {
        let node = db
            .node(&scopes, id.parse().unwrap())
            .unwrap_or_else(|| panic!("seeded id {id} vanished"));
        !node.props.contains_key("forgotten_at") && !node.props.contains_key("superseded_at")
    };
    let dropped: Vec<String> = judge["duplicates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["verdict"] == "consolidate")
        .map(|d| d["drop_id"].as_str().unwrap().to_string())
        .collect();
    for c in judge["decay"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        match c["verdict"].as_str().unwrap() {
            "forget" => assert!(!live(id), "verdict said forget but {id} is live"),
            "keep" if !dropped.contains(&id.to_string()) => {
                assert!(live(id), "verdict said keep but {id} is retired")
            }
            _ => {}
        }
    }
    for id in &dropped {
        assert!(!live(id), "consolidate said drop but {id} is live");
    }
    for id in judge["acted_ids"].as_array().unwrap() {
        assert!(!live(id.as_str().unwrap()), "acted_ids lists a live id");
    }
    // And the seeded ids all still EXIST (forget/consolidate are soft).
    for id in &seeded {
        let _ = live(id);
    }
}
