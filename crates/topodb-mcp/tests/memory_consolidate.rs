//! `consolidate_memories`: turn a near-duplicate PAIR (as surfaced by
//! `find_duplicate_memories`) into one canonical memory. The caller names which
//! survives (`keep`) and which is retired (`drop`) — never inferred from
//! similarity, because near-dup scores are topical, not factual (a contradicting
//! correction scores high too). `keep` inherits `drop`'s unique relationships so
//! no graph knowledge is lost, then `drop` is superseded (marked + edges closed),
//! all in one atomic batch. No embeddings needed — this is a graph operation, so
//! the whole suite runs in CI.

mod common;

use common::{Server, DEFAULT_TIMEOUT as T};
use serde_json::json;

const A: &str = "01HZY0AAAAAAAAAAAAAAAAAAAA";

fn fresh() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::spawn(&dir.path().join("t.redb"), &["--scope", A]);
    s.initialize(T);
    (dir, s)
}

fn id_of(v: &serde_json::Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

fn open_targets(edges: &serde_json::Value) -> Vec<String> {
    edges["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["valid_to"].is_null())
        .map(|e| e["to"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn consolidate_retires_drop_and_transfers_its_unique_edges() {
    let (_d, mut s) = fresh();
    let alpha = id_of(&s.call_tool_ok("create_entity", json!({"name": "Alpha"}), T));
    let beta = id_of(&s.call_tool_ok("create_entity", json!({"name": "Beta"}), T));

    // keep -> Alpha; drop -> Beta (a relationship keep does NOT have).
    let keep = id_of(&s.call_tool_ok("create_memory", json!({"content": "canonical fact"}), T));
    s.call_tool_ok(
        "link",
        json!({"from_id": keep, "to_id": alpha, "edge_type": "about"}),
        T,
    );
    let drop = id_of(&s.call_tool_ok("create_memory", json!({"content": "redundant fact"}), T));
    s.call_tool_ok(
        "link",
        json!({"from_id": drop, "to_id": beta, "edge_type": "about"}),
        T,
    );

    let res = s.call_tool_ok(
        "consolidate_memories",
        json!({"keep": keep, "drop": drop}),
        T,
    );
    assert_eq!(res["kept"], keep, "{res}");
    assert_eq!(res["dropped"], drop, "{res}");
    let transferred = res["transferred_edges"].as_array().unwrap();
    assert_eq!(
        transferred.len(),
        1,
        "the Beta link is unique to drop: {res}"
    );
    assert_eq!(transferred[0]["to"], beta, "{res}");

    // keep now carries BOTH its own Alpha edge and the inherited Beta edge.
    let ke = s.call_tool_ok("get_edges", json!({"from_id": keep}), T);
    let tos = open_targets(&ke);
    assert!(
        tos.contains(&alpha) && tos.contains(&beta),
        "keep should inherit the Beta link: {ke}"
    );

    // drop is retired: superseded_at stamped, its open edges closed.
    let dn = s.call_tool_ok("get_node", json!({"id": drop}), T);
    assert!(
        !dn["node"]["props"]["superseded_at"].is_null(),
        "drop must be marked superseded: {dn}"
    );
    let de = s.call_tool_ok("get_edges", json!({"from_id": drop, "open_only": true}), T);
    assert_eq!(
        de["edges"].as_array().unwrap().len(),
        0,
        "drop's open edges must be closed: {de}"
    );
}

#[test]
fn consolidate_does_not_duplicate_an_edge_keep_already_has() {
    let (_d, mut s) = fresh();
    let alpha = id_of(&s.call_tool_ok("create_entity", json!({"name": "Alpha"}), T));

    // BOTH memories link to Alpha — the classic true-duplicate case.
    let keep = id_of(&s.call_tool_ok("create_memory", json!({"content": "fact"}), T));
    s.call_tool_ok(
        "link",
        json!({"from_id": keep, "to_id": alpha, "edge_type": "about"}),
        T,
    );
    let drop = id_of(&s.call_tool_ok("create_memory", json!({"content": "same fact reworded"}), T));
    s.call_tool_ok(
        "link",
        json!({"from_id": drop, "to_id": alpha, "edge_type": "about"}),
        T,
    );

    let res = s.call_tool_ok(
        "consolidate_memories",
        json!({"keep": keep, "drop": drop}),
        T,
    );
    assert_eq!(
        res["transferred_edges"].as_array().unwrap().len(),
        0,
        "keep already links Alpha — nothing to transfer: {res}"
    );
    // keep must have exactly ONE open edge to Alpha, not a stacked duplicate.
    let ke = s.call_tool_ok(
        "get_edges",
        json!({"from_id": keep, "to_id": alpha, "open_only": true}),
        T,
    );
    assert_eq!(
        ke["edges"].as_array().unwrap().len(),
        1,
        "no duplicate keep->Alpha edge: {ke}"
    );
}

#[test]
fn consolidate_rejects_bad_input() {
    let (_d, mut s) = fresh();
    let keep = id_of(&s.call_tool_ok("create_memory", json!({"content": "keeper"}), T));
    let drop = id_of(&s.call_tool_ok("create_memory", json!({"content": "goner"}), T));
    let entity = id_of(&s.call_tool_ok("create_entity", json!({"name": "NotAMemory"}), T));

    // Same node for keep and drop.
    common::expect_tool_error(&s.call_tool(
        "consolidate_memories",
        json!({"keep": keep, "drop": keep}),
        T,
    ));
    // A non-existent drop.
    common::expect_tool_error(&s.call_tool(
        "consolidate_memories",
        json!({"keep": keep, "drop": "01HZY0BBBBBBBBBBBBBBBBBBBB"}),
        T,
    ));
    // A non-Memory node as drop.
    common::expect_tool_error(&s.call_tool(
        "consolidate_memories",
        json!({"keep": keep, "drop": entity}),
        T,
    ));

    // First consolidate retires drop; a second attempt on the retired drop errors.
    s.call_tool_ok(
        "consolidate_memories",
        json!({"keep": keep, "drop": drop}),
        T,
    );
    common::expect_tool_error(&s.call_tool(
        "consolidate_memories",
        json!({"keep": keep, "drop": drop}),
        T,
    ));
}
