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
    // The common helper defaults every test server to `--embeddings off`;
    // this test opts back in (config parsing is last-wins).
    let mut server = Server::spawn(
        &db_path,
        &[
            "--scope",
            scope.as_str(),
            "--embeddings",
            "bge-small-en-v1.5",
        ],
    );
    server.initialize(DEFAULT_TIMEOUT);

    // Wait for the model (download on first ever run; cached after).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let info = server.call_tool_ok("db_info", serde_json::json!({}), DEFAULT_TIMEOUT);
        match info["embeddings"]["status"].as_str().unwrap() {
            "ready" => break,
            "failed" => panic!("model failed to load: {info:#?}"),
            "off" => panic!("embedder is off — the spawn args failed to override the test default"),
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

/// One-shot generator for the committed golden corpus. Run manually:
/// cargo test -p topodb-mcp --test embeddings -- --ignored generate_recall_corpus
/// Reads corpus-src.json (texts + expectations, hand-written), computes
/// real embeddings for every node text and query, writes
/// crates/topodb/tests/fixtures/recall-corpus.json. Committed so ENGINE
/// tests never need the model.
#[test]
#[ignore]
fn generate_recall_corpus() {
    use std::path::Path;
    #[derive(serde::Deserialize, serde::Serialize)]
    struct Src {
        nodes: Vec<SrcNode>,
        synonyms: Vec<serde_json::Value>,
        queries: Vec<SrcQuery>,
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    struct SrcNode {
        key: String,
        label: String,
        text: String,
        links: Vec<String>,
    }
    #[derive(serde::Deserialize, serde::Serialize)]
    struct SrcQuery {
        query: String,
        expect_top3: Vec<String>,
    }

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../topodb/tests/fixtures");
    let src: Src = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join("recall-corpus-src.json")).unwrap(),
    )
    .unwrap();

    let mut model = fastembed::TextEmbedding::try_new(
        fastembed::TextInitOptions::new(fastembed::EmbeddingModel::BGESmallENV15)
            .with_cache_dir(dirs_or_home_cache()), // helper below
    )
    .expect("model must load (network needed on first run)");

    let node_texts: Vec<&str> = src.nodes.iter().map(|n| n.text.as_str()).collect();
    let node_vecs = model.embed(node_texts, None).unwrap();
    let query_texts: Vec<&str> = src.queries.iter().map(|q| q.query.as_str()).collect();
    let query_vecs = model.embed(query_texts, None).unwrap();

    let out = serde_json::json!({
        "model": "bge-small-en-v1.5",
        "nodes": src.nodes.iter().zip(&node_vecs).map(|(n, v)| serde_json::json!({
            "key": n.key, "label": n.label, "text": n.text, "links": n.links, "vector": v,
        })).collect::<Vec<_>>(),
        "synonyms": src.synonyms,
        "queries": src.queries.iter().zip(&query_vecs).map(|(q, v)| serde_json::json!({
            "query": q.query, "expect_top3": q.expect_top3, "vector": v,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(
        fixtures.join("recall-corpus.json"),
        serde_json::to_string(&out).unwrap(),
    )
    .unwrap();
}

fn dirs_or_home_cache() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/topodb/models"))
        .unwrap_or_else(|| std::path::PathBuf::from(".topodb-models"))
}
