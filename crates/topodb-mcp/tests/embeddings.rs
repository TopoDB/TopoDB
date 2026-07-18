//! Embedder lifecycle + degraded-mode tests. The real-model test is
//! #[ignore] (downloads ~34MB); everything else runs with --embeddings off.
mod common;
use common::{Server, DEFAULT_TIMEOUT};

#[test]
fn embeddings_off_reports_status_and_search_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("emb.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(
        &db_path,
        &["--scope", scope.as_str(), "--embeddings", "off"],
    );
    server.initialize(DEFAULT_TIMEOUT);

    let info = server.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT);
    assert_eq!(info["embeddings"]["status"], "off", "{info:#?}");

    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "degraded mode still stores and finds" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let hits = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "degraded" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(hits["hits"][0]["node"]["id"].as_str().unwrap(), m);
}
