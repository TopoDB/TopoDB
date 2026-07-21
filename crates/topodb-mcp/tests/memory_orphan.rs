//! `find_orphan_memories`: surface memories that are stored but connected to
//! nothing — a live Memory node with no OPEN outgoing edges. In this model a
//! memory joins the graph by linking out to entities (`remember` does this;
//! bare `create_memory` does not), so an orphan is only reachable by text/vector
//! search, never by traversal. Read-only and advisory: the caller decides
//! whether to link it or drop it. Superseded memories are excluded — their edges
//! are closed on purpose, they are retired, not orphaned. Pure graph op, no
//! embeddings, so the whole suite runs in CI.

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

fn orphan_ids(res: &serde_json::Value) -> Vec<String> {
    res["orphans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn surfaces_unlinked_memories_only() {
    let (_d, mut s) = fresh();
    let topic = id_of(&s.call_tool_ok("create_entity", json!({"name": "Topic"}), T));

    // Orphan: bare create_memory, never linked.
    let orphan = id_of(&s.call_tool_ok("create_memory", json!({"content": "floating fact"}), T));
    // Linked: create_memory + explicit link.
    let linked = id_of(&s.call_tool_ok("create_memory", json!({"content": "linked fact"}), T));
    s.call_tool_ok(
        "link",
        json!({"from_id": linked, "to_id": topic, "edge_type": "about"}),
        T,
    );
    // Linked: remember auto-links to its entity.
    let remembered = s.call_tool_ok(
        "remember",
        json!({"content": "remembered fact", "entities": ["Topic"]}),
        T,
    )["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = s.call_tool_ok("find_orphan_memories", json!({}), T);
    let ids = orphan_ids(&res);
    assert!(
        ids.contains(&orphan),
        "the unlinked memory is an orphan: {res}"
    );
    assert!(
        !ids.contains(&linked),
        "a linked memory is not an orphan: {res}"
    );
    assert!(
        !ids.contains(&remembered),
        "a remembered (auto-linked) memory is not an orphan: {res}"
    );
    assert_eq!(res["scanned"], 3, "three live memories examined: {res}");
    // Content is included so the caller can act without a follow-up read.
    let row = res["orphans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == orphan)
        .unwrap();
    assert_eq!(row["content"], "floating fact", "{res}");
}

#[test]
fn a_memory_whose_only_edge_is_closed_is_an_orphan() {
    let (_d, mut s) = fresh();
    let topic = id_of(&s.call_tool_ok("create_entity", json!({"name": "Topic"}), T));
    let mem = id_of(&s.call_tool_ok("create_memory", json!({"content": "briefly linked"}), T));
    let edge = id_of(&s.call_tool_ok(
        "link",
        json!({"from_id": mem, "to_id": topic, "edge_type": "about"}),
        T,
    ));

    // While the edge is open, it is NOT an orphan.
    assert!(!orphan_ids(&s.call_tool_ok("find_orphan_memories", json!({}), T)).contains(&mem));

    // Close the only edge -> now it links to nothing OPEN -> orphan.
    s.call_tool_ok("close_edge", json!({"id": edge}), T);
    assert!(
        orphan_ids(&s.call_tool_ok("find_orphan_memories", json!({}), T)).contains(&mem),
        "a memory with only closed edges is an orphan"
    );
}

#[test]
fn superseded_memories_are_not_orphans() {
    let (_d, mut s) = fresh();
    // A memory that gets retired (its edges close on supersession) must NOT be
    // reported as an orphan — it was retired on purpose, not left dangling.
    let old = id_of(&s.call_tool_ok("create_memory", json!({"content": "old fact"}), T));
    // Retire it by superseding with a replacement.
    s.call_tool_ok(
        "remember",
        json!({"content": "corrected fact", "entities": ["Topic"], "supersedes": [old]}),
        T,
    );

    let res = s.call_tool_ok("find_orphan_memories", json!({}), T);
    assert!(
        !orphan_ids(&res).contains(&old),
        "a superseded memory is retired, not an orphan: {res}"
    );
    assert_eq!(
        res["scanned"], 1,
        "only the live replacement is scanned: {res}"
    );
}

#[test]
fn empty_and_bounds() {
    let (_d, mut s) = fresh();
    // Empty db: empty list, not an error.
    let res = s.call_tool_ok("find_orphan_memories", json!({}), T);
    assert_eq!(res["orphans"].as_array().unwrap().len(), 0, "{res}");
    assert_eq!(res["scanned"], 0, "{res}");
    assert_eq!(res["truncated"], false, "{res}");

    // limit out of range is rejected.
    common::expect_tool_error(&s.call_tool("find_orphan_memories", json!({"limit": 0}), T));
    common::expect_tool_error(&s.call_tool("find_orphan_memories", json!({"limit": 1001}), T));
}
