//! Lean smoke test: spawn the real `topodb-mcp` binary and drive just the
//! MCP initialize handshake + `tools/list` over stdio JSON-RPC. Confirms the
//! server boots, negotiates, and advertises its full tool surface.
//!
//! The thorough scenario (write tools, read tools, error paths, restart
//! persistence) lives in `tests/e2e.rs` — this file stays intentionally
//! shallow as the fast-running "does the binary even come up" check. Shared
//! spawn/JSON-RPC/deadline plumbing lives in `tests/common/mod.rs` (see that
//! module's docs for the Windows-safety rationale).

mod common;

use common::{Server, DEFAULT_TIMEOUT};

#[test]
fn handshake_and_tools_list_exposes_all_tools() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("smoke.redb");
    let mut server = Server::spawn(&db_path, &[]);

    let init = server.initialize(DEFAULT_TIMEOUT);
    assert!(
        init.get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize result should advertise tools capability: {init}"
    );

    let tools = server.tools_list(DEFAULT_TIMEOUT);

    // db_info + 14 read tools + 12 write tools = 27 total.
    assert_eq!(
        tools.len(),
        27,
        "expected exactly 27 tools, got: {tools:#?}"
    );
    for name in [
        "db_info",
        "get_node",
        "find_by_prop",
        "search_memories",
        "recent_memories",
        "find_duplicate_memories",
        "traverse",
        "suggest_links",
        "access_stats",
        "get_changes",
        "get_edges",
        "create_memory",
        "create_entity",
        "remember",
        "link",
        "add_alias",
        "add_synonym",
    ] {
        let tool = tools
            .iter()
            .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
            .unwrap_or_else(|| panic!("tools/list must include {name}: {tools:#?}"));
        let description = tool
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert!(
            !description.is_empty(),
            "{name} must carry a non-empty description"
        );
    }
}
