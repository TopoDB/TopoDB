//! Full end-to-end MCP integration scenario (Plan 4 Task 6): spawns the real
//! `topodb-mcp` binary against a fresh tempdir db with an explicit `--scope
//! <fresh ULID>` (so this run's data can never collide with another test's),
//! then drives every tool over raw newline-delimited JSON-RPC exactly as a
//! real client would — including a hard restart on the same db file to prove
//! persistence survives the MCP process boundary, not just the engine layer
//! (that's already covered by `crates/topodb`'s own tests).
//!
//! Steps below are numbered to match the plan's Task 6 scenario 1-11. Shared
//! spawn/JSON-RPC/deadline plumbing lives in `tests/common/mod.rs` — see that
//! module's docs for why every read is deadlined and the child is always
//! killed (the Windows-safety rationale this file leans on throughout).

mod common;

use std::str::FromStr;
use std::time::{Duration, Instant};

use common::{expect_tool_error, structured_content, Server, DEFAULT_TIMEOUT};
use topodb::NodeId;

#[test]
fn end_to_end_scenario_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("e2e.redb");
    // A fresh scope ULID per run: isolates this test's data from anything
    // else that might touch the db file, and doubles as the "arbitrary valid
    // ULID" fixture for the malformed/bogus-id error-path assertions below.
    let scope = topodb::ScopeId::new().to_string();
    // --allow-unscoped-changes: this test drives get_changes exactly as a
    // legitimate sync host would (see Task 5's sanctioned carve-out).
    let scope_args = ["--scope", scope.as_str(), "--allow-unscoped-changes"];

    let mut server = Server::spawn(&db_path, &scope_args);

    // --- Step 1: handshake + tools/list -------------------------------
    let init = server.initialize(DEFAULT_TIMEOUT);
    assert!(
        init.get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize result should advertise tools capability: {init}"
    );

    let tools = server.tools_list(DEFAULT_TIMEOUT);
    assert_eq!(
        tools.len(),
        27,
        "expected exactly 27 tools (db_info + 14 read + 12 write), got: {tools:#?}"
    );
    for name in [
        "db_info",
        "get_node",
        "find_by_prop",
        "search_memories",
        "recent_memories",
        "find_duplicate_memories",
        "find_orphan_memories",
        "find_stale_memories",
        "memory_health",
        "traverse",
        "suggest_links",
        "access_stats",
        "get_changes",
        "get_edges",
        "create_memory",
        "remember",
        "create_entity",
        "link",
        "add_alias",
        "add_synonym",
        "set_node_props",
        "remove_node",
        "close_edge",
        "set_embedding",
        "search_vectors",
        "submit_batch",
        "consolidate_memories",
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

    // --- Step 2: create_entity {name: "ada"} -> id A -------------------
    let created = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "ada" }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        created.get("created"),
        Some(&serde_json::Value::Bool(true)),
        "first create_entity for a name must report created:true: {created:#?}"
    );
    let entity_id = created
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return a structured id")
        .to_string();

    // --- Step 2b: create_entity is find-or-create — a case/whitespace
    // variant of the same name resolves to the SAME node, created:false.
    let deduped = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "  Ada " }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        deduped.get("id").and_then(|v| v.as_str()),
        Some(entity_id.as_str()),
        "create_entity with a name variant must resolve to the existing \
         entity, not mint a duplicate: {deduped:#?}"
    );
    assert_eq!(
        deduped.get("created"),
        Some(&serde_json::Value::Bool(false)),
        "the deduped create_entity must report created:false: {deduped:#?}"
    );

    // --- Step 3: find_by_prop finds A -----------------------------------
    let found = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "ada" }),
        DEFAULT_TIMEOUT,
    );
    let nodes = found["nodes"].as_array().expect("nodes array");
    assert!(
        nodes.iter().any(|n| n["id"] == entity_id),
        "find_by_prop should locate the entity just created: {found:#?}"
    );

    // --- Step 4: create_memory -> id M ----------------------------------
    let memory_id = server
        .call_tool_ok(
            "create_memory",
            serde_json::json!({ "content": "ada wrote the first program" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_memory should return a structured id")
        .to_string();

    // --- Step 5: search_memories ranks M first with score > 0 -----------
    let search = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "first program" }),
        DEFAULT_TIMEOUT,
    );
    let hits = search["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "search_memories should return at least one hit: {search:#?}"
    );
    assert_eq!(
        hits[0]["node"]["id"], memory_id,
        "the just-created memory should rank first: {search:#?}"
    );
    let score = hits[0]["score"]
        .as_f64()
        .expect("hit score should be a number");
    assert!(
        score > 0.0,
        "top hit's BM25 score should be > 0, got {score}: {search:#?}"
    );

    // --- Step 6: link M -> A (edge_type "mentions") ---------------------
    let edge_id = server
        .call_tool_ok(
            "link",
            serde_json::json!({
                "from_id": memory_id,
                "to_id": entity_id,
                "edge_type": "mentions"
            }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("link should return a structured id")
        .to_string();

    // --- Step 6b: link is idempotent per (from, to, type) — repeating the
    // call (even with a casing/separator variant of the type) reuses the
    // open edge instead of stacking a parallel duplicate.
    let relink = server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": memory_id,
            "to_id": entity_id,
            "edge_type": "Mentions"
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        relink.get("id").and_then(|v| v.as_str()),
        Some(edge_id.as_str()),
        "re-linking the same (from, to, type) must return the existing edge: {relink:#?}"
    );
    assert_eq!(
        relink.get("created"),
        Some(&serde_json::Value::Bool(false)),
        "the deduped link must report created:false: {relink:#?}"
    );

    // --- Step 6c: get_edges surfaces the open edge by type ---------------
    let edges = server.call_tool_ok(
        "get_edges",
        serde_json::json!({ "from_id": memory_id, "edge_type": "mentions" }),
        DEFAULT_TIMEOUT,
    );
    let edge_rows = edges["edges"].as_array().expect("edges array");
    assert!(
        edge_rows
            .iter()
            .any(|e| e["id"] == edge_id && e["valid_to"].is_null()),
        "get_edges should list the open mentions edge: {edges:#?}"
    );

    // --- Step 7: traverse from A (default direction "both") reaches M ---
    let traverse = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": entity_id, "max_hops": 1 }),
        DEFAULT_TIMEOUT,
    );
    let subgraph = &traverse["subgraph"];
    let sg_nodes = subgraph["nodes"].as_array().expect("subgraph nodes");
    let sg_edges = subgraph["edges"].as_array().expect("subgraph edges");
    assert!(
        sg_nodes.iter().any(|n| n["id"] == memory_id),
        "traversing 1 hop from the entity should reach the linked memory \
         (edge direction is M->A, so this also exercises default \
         direction:both): {subgraph:#?}"
    );
    assert!(
        sg_edges.iter().any(|e| e["id"] == edge_id),
        "traverse should surface the mentions edge: {subgraph:#?}"
    );

    // --- Step 8: access_stats on M eventually shows access_count >= 1 ---
    // Counter bumps are fire-and-forget onto a background bumper thread that
    // flushes on a ~100ms cadence (see topodb::Db::bump / the bumper loop in
    // db.rs) — NOT synchronous with the read that triggered them. Both
    // search_memories (step 5) and traverse (step 7) returned M, so each
    // queued a bump; poll access_stats against a deadline instead of
    // asserting on the first call, which would flake under any scheduling
    // jitter.
    let poll_deadline = Instant::now() + Duration::from_secs(5);
    let access_count = loop {
        let stats = server.call_tool_ok(
            "access_stats",
            serde_json::json!({ "id": memory_id }),
            DEFAULT_TIMEOUT,
        );
        assert_eq!(
            stats.get("found"),
            Some(&serde_json::Value::Bool(true)),
            "access_stats should find the memory node: {stats:#?}"
        );
        if let Some(n) = stats.get("access_count").and_then(|v| v.as_u64()) {
            if n >= 1 {
                break n;
            }
        }
        assert!(
            Instant::now() < poll_deadline,
            "access_count never reached >= 1 within the poll deadline; last \
             access_stats result: {stats:#?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(access_count >= 1);

    // --- Step 9: get_changes since_seq=1 covers the creates + link ------
    // Op-log seqs are assigned in commit order starting at 1 (db_info: "0 on
    // a fresh db"): 1 = create_entity, 2 = create_memory, 3 = link. Rather
    // than assume the exact externally-tagged JSON shape of `Op`, assert on
    // substrings of each op's rendered JSON — robust to that shape while
    // still proving each op is the right op (it carries the right id) in
    // the right order.
    let changes = server.call_tool_ok(
        "get_changes",
        serde_json::json!({ "since_seq": 1 }),
        DEFAULT_TIMEOUT,
    );
    let ops = changes["ops"].as_array().expect("ops array");
    assert!(
        ops.len() >= 3,
        "get_changes since_seq=1 should cover at least the 3 writes this \
         test made (create_entity, create_memory, link): {ops:#?}"
    );
    // seqs strictly increasing and starting no earlier than since_seq.
    let seqs: Vec<u64> = ops
        .iter()
        .map(|o| o["seq"].as_u64().expect("seq should be a u64"))
        .collect();
    assert!(seqs[0] >= 1, "first seq should be >= since_seq: {seqs:?}");
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seqs should be strictly increasing: {seqs:?}"
    );
    let ops_json: Vec<String> = ops
        .iter()
        .map(|o| serde_json::to_string(&o["op"]).expect("op should serialize"))
        .collect();
    assert!(
        ops_json.iter().any(|s| s.contains(&entity_id)),
        "get_changes should include the create_entity op (id {entity_id}): {ops_json:#?}"
    );
    assert!(
        ops_json.iter().any(|s| s.contains(&memory_id)),
        "get_changes should include the create_memory op (id {memory_id}): {ops_json:#?}"
    );
    assert!(
        ops_json.iter().any(|s| s.contains(&edge_id)),
        "get_changes should include the link/CreateEdge op (id {edge_id}): {ops_json:#?}"
    );

    // --- Step 10: error paths — tool errors, not crashes -----------------

    // find_by_prop on an undeclared property: the default spec only equality
    // -indexes (Entity, name), so a different prop must be rejected rather
    // than silently returning nothing.
    let resp = server.call_tool(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "not_an_indexed_prop", "value": "ada" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // link with a bogus (well-formed but nonexistent) endpoint id.
    let bogus_id = NodeId::new().to_string();
    let resp = server.call_tool(
        "link",
        serde_json::json!({
            "from_id": bogus_id,
            "to_id": entity_id,
            "edge_type": "mentions"
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // malformed scope string on a read tool.
    let resp = server.call_tool(
        "get_node",
        serde_json::json!({ "id": entity_id, "scope": "not-a-valid-ulid-or-shared" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    // The server must still be alive and answering normally after all three
    // error paths — proof they were handled, not a crash the child happened
    // to survive by luck.
    let still_alive = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": entity_id }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        still_alive.get("found"),
        Some(&serde_json::Value::Bool(true)),
        "server should still answer correctly after the error paths: {still_alive:#?}"
    );

    // get_node's clean not-found path: a well-formed ULID that was never
    // created must come back as a SUCCESSFUL result with found:false and no
    // `node` field — NOT a tool error (this is server.rs's `Option::None`
    // branch of GetNodeResult, distinct from the malformed-input error paths
    // above). This is the one get_node behaviour the error paths and the
    // found:true liveness check don't cover.
    let never_created = NodeId::new().to_string();
    let not_found = server.call_tool_ok(
        "get_node",
        serde_json::json!({ "id": never_created }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        not_found.get("found"),
        Some(&serde_json::Value::Bool(false)),
        "get_node on a valid-but-nonexistent ULID should cleanly report \
         found:false, not error or crash: {not_found:#?}"
    );
    assert!(
        not_found.get("node").is_none(),
        "a not-found get_node result must not carry a node field: {not_found:#?}"
    );

    // --- Step 11: restart on the SAME db file; data persists ------------
    drop(server); // kills the child and waits for exit (see Server::drop)

    let mut server2 = Server::spawn(&db_path, &scope_args);
    server2.initialize(DEFAULT_TIMEOUT);

    let search_after_restart = structured_content(&server2.call_tool(
        "search_memories",
        serde_json::json!({ "query": "first program" }),
        DEFAULT_TIMEOUT,
    ));
    let hits_after_restart = search_after_restart["hits"].as_array().expect("hits array");
    assert!(
        hits_after_restart
            .iter()
            .any(|h| h["node"]["id"] == memory_id),
        "search_memories should still find M after a restart on the same \
         db file (persistence through the MCP layer): {search_after_restart:#?}"
    );

    // Bogus-id fixture sanity: `bogus_id` must be a genuine, well-formed
    // ULID so the step-10 `link` error path tested a *valid-but-nonexistent*
    // endpoint (engine `Rejected`), not a *malformed* one (parse error) —
    // two different error branches. Guard the actual invariant that matters:
    // NodeId Display/FromStr round-trips, so `bogus_id` parses back to the
    // exact id it was rendered from. (`.is_ok()` alone was true by
    // construction and guarded nothing.)
    let reparsed = NodeId::from_str(&bogus_id).expect("bogus_id must be a valid ULID");
    assert_eq!(
        reparsed.to_string(),
        bogus_id,
        "NodeId Display/FromStr should round-trip"
    );
}

#[test]
fn traverse_with_as_of_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("as_of_traverse.redb");
    let scope = topodb::ScopeId::new().to_string();
    let scope_args = ["--scope", scope.as_str(), "--allow-unscoped-changes"];

    let mut server = Server::spawn(&db_path, &scope_args);
    server.initialize(DEFAULT_TIMEOUT);

    // Create entity E1
    let e1_id = server
        .call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": "entity_one" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return id")
        .to_string();

    // Create memory M
    let memory_id = server
        .call_tool_ok(
            "create_memory",
            serde_json::json!({ "content": "temporal test memory" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_memory should return id")
        .to_string();

    // Link M -> E1
    let edge1_id = server
        .call_tool_ok(
            "link",
            serde_json::json!({
                "from_id": memory_id,
                "to_id": e1_id,
                "edge_type": "mentions"
            }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("link should return id")
        .to_string();

    // Guard ms-granularity by sleeping before capture
    std::thread::sleep(Duration::from_millis(5));

    // Capture `mid` BETWEEN E1 link creation and close_edge call
    let mid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Guard ms-granularity by sleeping before close
    std::thread::sleep(Duration::from_millis(5));

    // Close the E1 edge
    server.call_tool_ok(
        "close_edge",
        serde_json::json!({ "id": edge1_id }),
        DEFAULT_TIMEOUT,
    );

    // Create entity E2
    let e2_id = server
        .call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": "entity_two" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return id")
        .to_string();

    // Link M to E2
    let _edge2_id = server
        .call_tool_ok(
            "link",
            serde_json::json!({
                "from_id": memory_id,
                "to_id": e2_id,
                "edge_type": "mentions"
            }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("link should return id")
        .to_string();

    // Traverse WITHOUT as_of should contain E2 and NOT E1
    let traverse_current = server.call_tool_ok(
        "traverse",
        serde_json::json!({
            "seed_id": memory_id,
            "max_hops": 1
        }),
        DEFAULT_TIMEOUT,
    );
    let sg_current = &traverse_current["subgraph"];
    let nodes_current = sg_current["nodes"].as_array().expect("subgraph nodes");
    let current_ids: Vec<String> = nodes_current
        .iter()
        .filter_map(|n| n["id"].as_str().map(|s| s.to_string()))
        .collect();

    assert!(
        current_ids.contains(&e2_id),
        "traverse WITHOUT as_of must contain E2's id: nodes={:?}",
        current_ids
    );
    assert!(
        !current_ids.contains(&e1_id),
        "traverse WITHOUT as_of must NOT contain E1's id (edge is closed): nodes={:?}",
        current_ids
    );

    // Traverse WITH as_of=mid should contain E1 and NOT E2
    let traverse_historical = server.call_tool_ok(
        "traverse",
        serde_json::json!({
            "seed_id": memory_id,
            "max_hops": 1,
            "as_of": mid
        }),
        DEFAULT_TIMEOUT,
    );
    let sg_historical = &traverse_historical["subgraph"];
    let nodes_historical = sg_historical["nodes"].as_array().expect("subgraph nodes");
    let historical_ids: Vec<String> = nodes_historical
        .iter()
        .filter_map(|n| n["id"].as_str().map(|s| s.to_string()))
        .collect();

    assert!(
        historical_ids.contains(&e1_id),
        "traverse WITH as_of=mid must contain E1's id (edge was open at mid): nodes={:?}",
        historical_ids
    );
    assert!(
        !historical_ids.contains(&e2_id),
        "traverse WITH as_of=mid must NOT contain E2's id (linked after mid): nodes={:?}",
        historical_ids
    );

    // Test: as_of with invalid value (non-positive) should error
    let invalid_as_of = server.call_tool(
        "traverse",
        serde_json::json!({
            "seed_id": memory_id,
            "as_of": 0
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&invalid_as_of);

    let invalid_as_of_neg = server.call_tool(
        "traverse",
        serde_json::json!({
            "seed_id": memory_id,
            "as_of": -5
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&invalid_as_of_neg);
}

#[test]
fn get_edges_with_as_of_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("as_of_edges.redb");
    let scope = topodb::ScopeId::new().to_string();
    let scope_args = ["--scope", scope.as_str(), "--allow-unscoped-changes"];

    let mut server = Server::spawn(&db_path, &scope_args);
    server.initialize(DEFAULT_TIMEOUT);

    // Create entity E1
    let e1_id = server
        .call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": "edge_test_one" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return id")
        .to_string();

    // Create memory M
    let memory_id = server
        .call_tool_ok(
            "create_memory",
            serde_json::json!({ "content": "edge temporal test" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_memory should return id")
        .to_string();

    // Create link M -> E1
    let edge1_id = server
        .call_tool_ok(
            "link",
            serde_json::json!({
                "from_id": memory_id,
                "to_id": e1_id,
                "edge_type": "links"
            }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("link should return id")
        .to_string();

    // Guard ms-granularity by sleeping before capture
    std::thread::sleep(Duration::from_millis(5));

    // Capture `mid` BETWEEN E1 link creation and close_edge call
    let mid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Guard ms-granularity by sleeping before close
    std::thread::sleep(Duration::from_millis(5));

    // Close the E1 edge
    server.call_tool_ok(
        "close_edge",
        serde_json::json!({ "id": edge1_id }),
        DEFAULT_TIMEOUT,
    );

    // Create entity E2
    let e2_id = server
        .call_tool_ok(
            "create_entity",
            serde_json::json!({ "name": "edge_test_two" }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create_entity should return id")
        .to_string();

    // Link M to E2
    let edge2_id = server
        .call_tool_ok(
            "link",
            serde_json::json!({
                "from_id": memory_id,
                "to_id": e2_id,
                "edge_type": "links"
            }),
            DEFAULT_TIMEOUT,
        )
        .get("id")
        .and_then(|v| v.as_str())
        .expect("link should return id")
        .to_string();

    // Capture current time after E2 is linked
    let future_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Get all edges (history: both closed and open)
    let all_edges_resp = server.call_tool_ok(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "edge_type": "links",
            "open_only": false
        }),
        DEFAULT_TIMEOUT,
    );
    let all_edges = all_edges_resp["edges"].as_array().expect("edges array");
    assert_eq!(
        all_edges.len(),
        2,
        "get_edges with open_only:false should return both edges (closed E1 + open E2): {:?}",
        all_edges
            .iter()
            .map(|e| e["id"].as_str())
            .collect::<Vec<_>>()
    );

    // Find the closed edge's valid_to to verify exclusive boundary
    let closed_edge = all_edges
        .iter()
        .find(|e| e["id"].as_str() == Some(edge1_id.as_str()))
        .expect("closed edge should be in results");
    let closed_valid_to = closed_edge["valid_to"]
        .as_i64()
        .expect("closed edge should have valid_to");

    // Capture the E1 edge's valid_from for boundary testing
    let e1_valid_from = all_edges
        .iter()
        .find(|e| e["id"].as_str() == Some(edge1_id.as_str()))
        .and_then(|e| e["valid_from"].as_i64())
        .expect("E1 edge should have valid_from");

    // Test 1: get_edges(from=M) with as_of=mid returns EXACTLY the E1 edge
    // DOCUMENTED CONTRACT: omit open_only when passing as_of
    let mid_edges_resp = server.call_tool_ok(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "edge_type": "links",
            "as_of": mid
        }),
        DEFAULT_TIMEOUT,
    );
    let mid_edges = mid_edges_resp["edges"].as_array().expect("edges array");
    assert_eq!(
        mid_edges.len(),
        1,
        "get_edges with as_of=mid should return exactly 1 edge (E1 only, E2 not created yet): {:?}",
        mid_edges
            .iter()
            .map(|e| e["id"].as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        mid_edges[0]["id"].as_str().unwrap(),
        edge1_id,
        "edge at mid timestamp must be the E1 edge"
    );
    assert_eq!(
        mid_edges[0]["to"].as_str().unwrap(),
        e1_id,
        "edge at mid must point to E1"
    );

    // Test 2: WITHOUT as_of but open_only unset returns both edges (history)
    let history_edges_resp = server.call_tool_ok(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "edge_type": "links",
            "open_only": false
        }),
        DEFAULT_TIMEOUT,
    );
    let history_edges = history_edges_resp["edges"].as_array().expect("edges array");
    assert_eq!(
        history_edges.len(),
        2,
        "get_edges with open_only:false returns complete history"
    );

    // Test 3: with as_of AFTER the close and after E2's link returns only E2's edge
    // DOCUMENTED CONTRACT: omit open_only when passing as_of
    let future_edges_resp = server.call_tool_ok(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "edge_type": "links",
            "as_of": future_time
        }),
        DEFAULT_TIMEOUT,
    );
    let future_edges = future_edges_resp["edges"].as_array().expect("edges array");
    assert_eq!(
        future_edges.len(),
        1,
        "get_edges with as_of after close should return only E2's edge: {:?}",
        future_edges
            .iter()
            .map(|e| e["id"].as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        future_edges[0]["id"].as_str().unwrap(),
        edge2_id,
        "edge at future time must be the E2 edge"
    );
    assert_eq!(
        future_edges[0]["to"].as_str().unwrap(),
        e2_id,
        "edge at future must point to E2"
    );

    // Test 4: EXCLUSIVE boundary — as_of == edge's valid_to must NOT include the edge
    // DOCUMENTED CONTRACT: omit open_only when passing as_of
    let boundary_edges_resp = server.call_tool_ok(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "edge_type": "links",
            "as_of": closed_valid_to
        }),
        DEFAULT_TIMEOUT,
    );
    let boundary_edges = boundary_edges_resp["edges"]
        .as_array()
        .expect("edges array");
    // At the exact valid_to boundary, the edge must be excluded (valid_from <= t < valid_to)
    assert!(
        !boundary_edges
            .iter()
            .any(|e| e["id"].as_str() == Some(edge1_id.as_str())),
        "get_edges with as_of == valid_to must NOT include the closed edge (exclusive boundary)"
    );

    // Test 5: INCLUSIVE boundary — as_of == edge's valid_from MUST include the edge
    let inclusive_boundary_resp = server.call_tool_ok(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "edge_type": "links",
            "as_of": e1_valid_from
        }),
        DEFAULT_TIMEOUT,
    );
    let inclusive_boundary_edges = inclusive_boundary_resp["edges"]
        .as_array()
        .expect("edges array");
    assert!(
        inclusive_boundary_edges
            .iter()
            .any(|e| e["id"].as_str() == Some(edge1_id.as_str())),
        "get_edges with as_of == valid_from must INCLUDE the edge (inclusive lower boundary)"
    );

    // Helper to extract error message from either JSON-RPC protocol error or tool result error
    fn extract_error_msg(resp: &serde_json::Value) -> String {
        resp.get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or(
                resp.get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(""),
            )
            .to_string()
    }

    // Test 6: as_of + open_only=true should error with "omit open_only" message
    let mutually_exclusive_true = server.call_tool(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "as_of": mid,
            "open_only": true
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&mutually_exclusive_true);
    let error_msg = extract_error_msg(&mutually_exclusive_true);
    assert!(
        error_msg.contains("omit open_only"),
        "as_of + open_only=true error must mention 'omit open_only': {}",
        error_msg
    );

    // Test 7: as_of + open_only=false should error with "omit open_only" message
    let mutually_exclusive_false = server.call_tool(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "as_of": mid,
            "open_only": false
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&mutually_exclusive_false);
    let error_msg = extract_error_msg(&mutually_exclusive_false);
    assert!(
        error_msg.contains("omit open_only"),
        "as_of + open_only=false error must mention 'omit open_only': {}",
        error_msg
    );

    // Test 8: invalid as_of (negative) should error with "positive Unix-millisecond" message
    // (validate_as_of must run first, before exclusivity check)
    let invalid_as_of = server.call_tool(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "as_of": -5
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&invalid_as_of);
    let error_msg = extract_error_msg(&invalid_as_of);
    assert!(
        error_msg.contains("positive Unix-millisecond"),
        "as_of < 0 error must contain 'positive Unix-millisecond': {}",
        error_msg
    );

    // Test 9: invalid as_of (zero) should error with "positive Unix-millisecond" message
    let invalid_as_of_zero = server.call_tool(
        "get_edges",
        serde_json::json!({
            "from_id": memory_id,
            "as_of": 0
        }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&invalid_as_of_zero);
    let error_msg = extract_error_msg(&invalid_as_of_zero);
    assert!(
        error_msg.contains("positive Unix-millisecond"),
        "as_of == 0 error must contain 'positive Unix-millisecond': {}",
        error_msg
    );
}
