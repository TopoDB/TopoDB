//! Behavioral tests for the `ingest_vault` + `seed_vault` tools (Task 12).
mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db, &["--scope", &scope]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

#[test]
fn ingest_then_seed_round_trips_over_mcp() {
    let (dir, mut server) = fresh_server();
    let vault = dir.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(
        vault.join("fact.md"),
        "---\nstatus: open\n---\nOmega ships [[widgets]].\n",
    )
    .unwrap();

    let r = server.call_tool_ok(
        "ingest_vault",
        serde_json::json!({ "vault": vault.to_str().unwrap() }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        (
            r["ingested"].as_u64(),
            r["errors"].as_array().unwrap().len()
        ),
        (Some(1), 0)
    );
    assert!(std::fs::read_to_string(vault.join("fact.md"))
        .unwrap()
        .contains("topodb-id:"));

    // Memory now recallable through the normal surface.
    let hits = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "Omega widgets" }),
        DEFAULT_TIMEOUT,
    );
    assert!(!hits["hits"].as_array().unwrap().is_empty());

    let seeded = dir.path().join("wm");
    let s = server.call_tool_ok(
        "seed_vault",
        serde_json::json!({ "vault": seeded.to_str().unwrap(), "entity": "widgets" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        (s["seeded"].clone(), s["stubs"].clone()),
        (serde_json::json!(1), serde_json::json!(1))
    );

    // Re-ingest of the seeded vault is pure skips (fixpoint over MCP too).
    let r2 = server.call_tool_ok(
        "ingest_vault",
        serde_json::json!({ "vault": seeded.to_str().unwrap() }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        (r2["ingested"].clone(), r2["superseded"].clone()),
        (serde_json::json!(0), serde_json::json!(0))
    );
}

#[test]
fn seed_vault_rejects_bad_selectors_and_unknown_fields() {
    let (dir, mut server) = fresh_server();
    let v = dir.path().join("wm");
    let both = server.call_tool(
        "seed_vault",
        serde_json::json!({ "vault": v.to_str().unwrap(), "query": "x", "entity": "y" }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&both);
    let neither = server.call_tool(
        "seed_vault",
        serde_json::json!({ "vault": v.to_str().unwrap() }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&neither);
    let unknown = server.call_tool(
        "ingest_vault",
        serde_json::json!({ "vault": v.to_str().unwrap(), "nope": 1 }),
        DEFAULT_TIMEOUT,
    );
    common::expect_tool_error(&unknown);
}
