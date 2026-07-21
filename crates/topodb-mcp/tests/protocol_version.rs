//! Multi-client protocol compatibility: the server is the memory backend for
//! several MCP clients (Claude Code, Pi, Codex), and those clients pin
//! different MCP protocol versions. The handshake must succeed — negotiate a
//! version and advertise the tool surface — for every real version a client
//! sends, not just the single string the rest of the suite happens to use.

mod common;

use common::{Server, DEFAULT_TIMEOUT};

/// Protocol versions the MCP spec has shipped and that real clients pin.
const CLIENT_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

#[test]
fn negotiates_every_protocol_version_a_real_client_sends() {
    for &version in CLIENT_VERSIONS {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("proto.redb");
        let mut server = Server::spawn(&db_path, &[]);

        let init = server.initialize_with_version(version, DEFAULT_TIMEOUT);

        // The handshake must return a usable session for this client: a
        // negotiated protocolVersion and the tools capability, so the client
        // can go on to list and call memory tools.
        let negotiated = init.get("protocolVersion").and_then(|v| v.as_str());
        assert!(
            negotiated.is_some(),
            "client version {version}: initialize must return a protocolVersion, got {init}"
        );
        assert!(
            init.get("capabilities")
                .and_then(|c| c.get("tools"))
                .is_some(),
            "client version {version}: session must advertise the tools capability, got {init}"
        );

        // And the memory surface must actually be reachable after that
        // handshake — a successful init that exposed no tools would be useless
        // to an agent.
        let tools = server.tools_list(DEFAULT_TIMEOUT);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        for must in ["remember", "search_memories", "traverse"] {
            assert!(
                names.contains(&must),
                "client version {version}: '{must}' must be listed, got {names:?}"
            );
        }
    }
}
