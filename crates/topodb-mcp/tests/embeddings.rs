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

#[test]
fn writes_never_block_on_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("noblock.redb");
    let scope = topodb::ScopeId::new().to_string();
    // Point the model dir somewhere unusable so init fails fast.
    let mut server = Server::spawn(
        &db_path,
        &[
            "--scope",
            scope.as_str(),
            "--model-dir",
            "/dev/null/not-a-dir",
        ],
    );
    server.initialize(DEFAULT_TIMEOUT);
    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "written while embedder is failing" }),
        DEFAULT_TIMEOUT,
    );
    assert!(m["id"].as_str().is_some());
}

/// Requires network on first run (~34MB model download) AND an installed
/// ONNX Runtime dylib (fastembed is built with `ort-load-dynamic`; e.g.
/// `brew install onnxruntime`) — without one, `status()` lands in `Failed`
/// rather than `Ready` and this test panics by design. Run locally:
/// cargo test -p topodb-mcp --test embeddings -- --ignored
#[test]
#[ignore]
fn real_model_semantic_recall_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("semantic.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);

    // Wait for the model (download on first ever run; cached after).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let info = server.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT);
        match info["embeddings"]["status"].as_str().unwrap() {
            "ready" => break,
            "failed" => panic!("model failed to load: {info:#?}"),
            _ => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "model never became ready"
                );
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }

    let m = server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "the login password rotates every ninety days" }),
        DEFAULT_TIMEOUT,
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Zero token overlap with the stored text (stems don't match either):
    let hits = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "credentials expiry", "fuzzy": false }),
        DEFAULT_TIMEOUT,
    );
    let ids: Vec<&str> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["node"]["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&m.as_str()),
        "semantic-only match must surface: {hits:#?}"
    );
}
