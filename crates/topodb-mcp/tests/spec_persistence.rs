//! Regression: `topodb-mcp` reopening an existing db WITHOUT `--spec` must
//! inherit the db's persisted index spec, not silently overwrite it with the
//! built-in default. The old `main` always called `Db::open_with(default_spec)`
//! when `--spec` was omitted, so pointing the server at an existing custom-spec
//! db without the flag reindexed it and dropped its declared equality indexes.
//!
//! Signal: a `(Person, handle)` equality lookup. The CUSTOM spec declares it
//! (Ok, empty); the built-in DEFAULT spec (equality on `(Entity, name)`) does
//! NOT — an undeclared lookup is a tool error. So a clobbered reopen flips this
//! probe from success to error, and the inverse `(Entity, name)` probe from
//! error to success. Asserting both pins down that we inherited the CUSTOM
//! spec specifically, not merely "a spec that accepts our probe".

mod common;

use common::{expect_tool_error, Server, DEFAULT_TIMEOUT};

#[test]
fn reopen_without_spec_preserves_persisted_custom_spec() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("custom.redb");
    let spec_path = dir.path().join("spec.json");
    // Custom spec: equality on (Person, handle) — a pair the default spec does
    // not declare, and no text index at all.
    std::fs::write(
        &spec_path,
        r#"{"equality":[{"label":"Person","prop":"handle"}],"text":[]}"#,
    )
    .unwrap();

    // First open: create the db WITH the custom spec, which persists it. No
    // writes needed — `ensure_index_spec` records the spec on open.
    {
        let mut server = Server::spawn(&db_path, &["--spec", spec_path.to_str().unwrap()]);
        server.initialize(DEFAULT_TIMEOUT);
        // Sanity: (Person, handle) is declared under the custom spec, so the
        // lookup succeeds (empty, but not a tool error).
        let ok = server.call_tool_ok(
            "find_by_prop",
            serde_json::json!({ "label": "Person", "prop": "handle", "value": "ada" }),
            DEFAULT_TIMEOUT,
        );
        assert!(
            ok["nodes"].as_array().expect("nodes array").is_empty(),
            "no Person nodes were written yet, expected empty: {ok:#?}"
        );
    } // server dropped -> child killed, db file closed

    // Second open: reopen the SAME db WITHOUT `--spec`. Must inherit the
    // persisted custom spec (via `open_stored`), NOT reset to the default.
    let mut server = Server::spawn(&db_path, &[]);
    server.initialize(DEFAULT_TIMEOUT);

    // (Person, handle) must STILL be a declared equality index. A tool error
    // here means the no-`--spec` reopen clobbered the persisted spec back to
    // the default — the exact regression this test guards.
    let reopened = server.call_tool_ok(
        "find_by_prop",
        serde_json::json!({ "label": "Person", "prop": "handle", "value": "ada" }),
        DEFAULT_TIMEOUT,
    );
    assert!(
        reopened["nodes"]
            .as_array()
            .expect("nodes array")
            .is_empty(),
        "(Person, handle) must remain declared after a no-`--spec` reopen: {reopened:#?}"
    );

    // Inverse: the DEFAULT spec declares (Entity, name), which the custom spec
    // does not. Under the correctly-inherited custom spec this lookup must be
    // rejected — proving we inherited the custom spec, not silently the default.
    let entity_probe = server.call_tool(
        "find_by_prop",
        serde_json::json!({ "label": "Entity", "prop": "name", "value": "ada" }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&entity_probe);
}
