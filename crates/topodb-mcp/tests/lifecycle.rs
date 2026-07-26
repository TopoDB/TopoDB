//! Behavioral tests for the `lifecycle_candidates` tool (spec:
//! docs/superpowers/specs/2026-07-25-memory-lifecycle-design.md, Phase C).
mod common;
use common::{Server, DEFAULT_TIMEOUT};

fn fresh_server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("lifecycle.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut server = Server::spawn(&db_path, &["--scope", scope.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);
    (dir, server)
}

fn ulid_ms(id: &str) -> i64 {
    let alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    id.chars()
        .take(10)
        .fold(0i64, |acc, c| acc * 32 + alphabet.find(c).unwrap() as i64)
}

#[test]
fn lifecycle_candidates_ranks_by_kind_and_skips_tombstoned() {
    let (_dir, mut server) = fresh_server();
    let remember = |server: &mut Server, content: &str, kind: Option<&str>| -> String {
        let mut params = serde_json::json!({ "content": content, "entities": ["topic"] });
        if let Some(k) = kind {
            params["kind"] = serde_json::json!(k);
        }
        server.call_tool_ok("remember", params, DEFAULT_TIMEOUT)["memory_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let ep = remember(&mut server, "ci was red this morning", Some("episodic"));
    let se = remember(&mut server, "release tags are per package", None);
    let dead = remember(&mut server, "stale duplicate", None);
    server.call_tool_ok(
        "forget",
        serde_json::json!({ "ids": [dead] }),
        DEFAULT_TIMEOUT,
    );

    let now = ulid_ms(&ep) + 28 * 86_400_000;
    let r = server.call_tool_ok(
        "lifecycle_candidates",
        serde_json::json!({ "now_ms": now }),
        DEFAULT_TIMEOUT,
    );
    let ids: Vec<&str> = r["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![ep.as_str(), se.as_str()],
        "episodic stalest; forgotten absent"
    );
    assert_eq!(r["candidates"][0]["kind"], "episodic");
    assert!(
        r["candidates"][0]["staleness"].as_f64().unwrap()
            > r["candidates"][1]["staleness"].as_f64().unwrap()
    );
    assert!(r["candidates"][0]["content"]
        .as_str()
        .unwrap()
        .contains("ci was red"));

    // Determinism under the injected clock.
    let again = server.call_tool_ok(
        "lifecycle_candidates",
        serde_json::json!({ "now_ms": now }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(r, again);

    // limit truncates after ranking.
    let top1 = server.call_tool_ok(
        "lifecycle_candidates",
        serde_json::json!({ "now_ms": now, "limit": 1 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(top1["candidates"].as_array().unwrap().len(), 1);
    assert_eq!(top1["candidates"][0]["id"].as_str().unwrap(), ep.as_str());
}

#[test]
fn lifecycle_candidates_rejects_bad_params_and_never_bumps() {
    let (_dir, mut server) = fresh_server();
    let mem = server.call_tool_ok(
        "remember",
        serde_json::json!({ "content": "quiet fact", "entities": ["topic"] }),
        DEFAULT_TIMEOUT,
    )["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    for (params, needle) in [
        (serde_json::json!({ "limit": 0 }), "limit"),
        (
            serde_json::json!({ "half_life_semantic_days": 0.0 }),
            "half-lives must be positive",
        ),
    ] {
        let resp = server.call_tool("lifecycle_candidates", params, DEFAULT_TIMEOUT);
        common::expect_tool_error(&resp);
        assert!(
            resp.to_string().contains(needle),
            "{needle:?} not in {resp}"
        );
    }

    // The sweep is unbumped: run it, then read access_stats — still zero.
    // (access_stats itself is documented non-bumping, so this read is safe
    // evidence; the async-flush race is covered by the json-layer fence
    // test — here we assert the steady state after a sweep.)
    server.call_tool_ok(
        "lifecycle_candidates",
        serde_json::json!({}),
        DEFAULT_TIMEOUT,
    );
    let stats = server.call_tool_ok(
        "access_stats",
        serde_json::json!({ "id": mem }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        stats["access_count"].as_u64(),
        Some(0),
        "sweep must not bump"
    );
}
