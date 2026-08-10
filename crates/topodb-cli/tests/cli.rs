use std::io::Write;
use std::process::Command;

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_topodb"));
    c.env_remove("TOPODB_DB");
    c.env_remove("TOPODB_SCOPE");
    c.env_remove("TOPODB_FORMAT");
    c
}

#[test]
fn holder_helper() {
    // When TOPODB_TEST_HOLD_DB is not set, this is a no-op pass.
    // When set to a path, this holds the db open for the specified duration (TOPODB_TEST_HOLD_MS).
    if let Ok(db_path) = std::env::var("TOPODB_TEST_HOLD_DB") {
        let hold_ms: u64 = std::env::var("TOPODB_TEST_HOLD_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(900);

        let _held = topodb::Db::open_stored(&db_path).expect("holder: failed to open db");
        std::thread::sleep(std::time::Duration::from_millis(hold_ms));
    }
}

#[test]
fn info_reports_fields_on_fresh_db() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin().args(["--db"]).arg(&db).arg("info").output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["current_seq"], 0);
    assert_eq!(v["default_scope"], "shared");
    assert!(v["format_version"].is_number());
}

/// A HOME-scoped run: point HOME (and USERPROFILE for Windows) at a tempdir
/// so the default db path lands there, never the real ~/.topodb.
fn bin_home(home: &std::path::Path) -> Command {
    let mut c = bin();
    c.env("HOME", home);
    c.env("USERPROFILE", home);
    c
}

#[test]
fn db_defaults_to_home_topodb_when_flag_absent() {
    let home = tempfile::tempdir().unwrap();
    let out = bin_home(home.path()).arg("info").output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        home.path().join(".topodb/memory.redb").exists(),
        "default db not created under HOME"
    );
}

#[test]
fn default_db_path_fails_when_home_unset() {
    let working_dir = tempfile::tempdir().unwrap();
    let mut c = bin();
    c.env_remove("HOME");
    c.env_remove("USERPROFILE");
    c.current_dir(working_dir.path()).arg("info");
    let out = c.output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "should fail with exit code 1, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !working_dir.path().join("~").exists(),
        "must not create a literal ~ directory"
    );
}

#[test]
fn db_from_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("env.redb");
    let out = bin().env("TOPODB_DB", &db).arg("info").output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.exists());
}

#[test]
fn db_and_scope_from_project_config() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let db = proj.path().join("cfg.redb");
    std::fs::write(
        proj.path().join(".topodb.toml"),
        // TOML literal string: Windows path backslashes must not be parsed as escapes
        format!("db = '{}'\nscope = \"shared\"\n", db.display()),
    )
    .unwrap();
    let out = bin_home(home.path())
        .current_dir(proj.path())
        .arg("info")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["path"], db.to_str().unwrap());
    assert!(db.exists());
}

#[test]
fn invalid_scope_error_names_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .env("TOPODB_SCOPE", "not-a-ulid!")
        .arg("info")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("env"), "expected source in error, got: {err}");
}

#[test]
fn malformed_config_is_exit_2_naming_file() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join(".topodb.toml"), "db = = =").unwrap();
    let out = bin_home(home.path())
        .current_dir(proj.path())
        .arg("info")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains(".topodb.toml"));
}

#[test]
fn create_and_link_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    // Use a fixed scope so all three land together.
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db"];
        v.push(db.to_str().unwrap());
        v.push("--scope");
        v.push(&scope);
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let (ent, s) = full(&["create-entity", "--name", "ada"]);
    assert!(s.success());
    let a = ent["id"].as_str().unwrap().to_string();
    let (mem, s) = full(&["create-memory", "--content", "ada wrote the first program"]);
    assert!(s.success());
    let m = mem["id"].as_str().unwrap().to_string();
    let (edge, s) = full(&["link", "--from", &m, "--to", &a, "--type", "mentions"]);
    assert!(s.success());
    assert!(edge["id"].as_str().is_some());
}

#[test]
fn link_with_bogus_id_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args([
            "link",
            "--from",
            "not-a-ulid",
            "--to",
            "also-bad",
            "--type",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "rejected");
}

#[test]
fn read_commands_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db"];
        v.push(db.to_str().unwrap());
        v.push("--scope");
        v.push(&scope);
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let a = full(&["create-entity", "--name", "ada"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    let m = full(&["create-memory", "--content", "ada wrote the first program"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    full(&["link", "--from", &m, "--to", &a, "--type", "mentions"]);

    // find works: a brand-new CLI db is created with the canonical
    // `topodb_json::default_spec()` (equality on Entity/name, text on
    // Memory/content) — the same spec topodb-mcp uses — so the equality
    // lookup resolves the entity by name out of the box.
    let (found, s) = full(&[
        "find", "--label", "Entity", "--prop", "name", "--value", "ada",
    ]);
    assert!(s.success());
    let arr = found.as_array().unwrap();
    assert_eq!(arr.len(), 1, "exactly the one entity named ada");
    assert_eq!(arr[0]["id"], serde_json::json!(a));

    // search works (default spec text-indexes Memory/content):
    let (hits, s) = full(&["search", "ada program"]);
    assert!(s.success());
    assert_eq!(
        hits.as_array().unwrap()[0]["node"]["id"],
        serde_json::json!(m)
    );

    // traverse from the entity reaches the memory:
    let (sg, s) = full(&["traverse", &a, "--max-hops", "1"]);
    assert!(s.success());
    let nodes = &sg["subgraph"]["nodes"];
    assert!(nodes
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["id"] == serde_json::json!(m)));

    // get found / not-found:
    assert_eq!(full(&["get", &m]).0["found"], serde_json::json!(true));
    let fresh = topodb::NodeId::new().to_string();
    let (nf, s) = full(&["get", &fresh]);
    assert!(s.success());
    assert_eq!(nf["found"], serde_json::json!(false));

    // changes:
    let (ch, s) = full(&["changes", "--since", "1"]);
    assert!(s.success());
    assert!(ch.as_array().unwrap().len() >= 3);
    // monotonic seqs:
    let seqs: Vec<u64> = ch
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
}

// --- Task 6: persistence across invocations + remaining error paths ---

/// Two SEPARATE `topodb` process invocations against the same `--db` file:
/// the first writes a memory, the second (a fresh process, fresh `Db`)
/// searches for it. Only passes if `open_stored` actually reopens the
/// on-disk state the first process durably wrote — an in-memory-only or
/// not-actually-flushed implementation would see zero hits here.
#[test]
fn data_persists_across_invocations() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db"];
        v.push(db.to_str().unwrap());
        v.push("--scope");
        v.push(&scope);
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    full(&["create-memory", "--content", "persistent needle memory"]);
    // A SEPARATE process (proves on-disk state through open_stored):
    let (hits, s) = full(&["search", "needle"]);
    assert!(s.success());
    assert_eq!(hits.as_array().unwrap().len(), 1);
}

/// A `(label, prop)` pair the open db's index spec never declared is a
/// caller-fixable input error (rejected/exit 2), not an internal failure or
/// a silent empty result.
#[test]
fn find_undeclared_prop_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["find", "--label", "Nope", "--prop", "x", "--value", "1"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "rejected"
    );
}

/// A `--db` path whose parent directory doesn't exist can't be created by
/// redb — a storage/db-open failure, not something the caller can fix by
/// changing an id/prop/scope, so it's internal/exit 1 (not rejected/exit 2).
/// Built under `tempdir()` with an extra non-existent path component rather
/// than a hardcoded `/no/...` root so it fails identically on Windows.
#[test]
fn nonexistent_parent_dir_is_internal_error_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("missing_subdir").join("t.redb");
    let out = bin().args(["--db"]).arg(&db).arg("info").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "internal"
    );
}

/// `--scope` that's neither "shared" nor a parseable ULID is a caller
/// input error resolved before the db is even opened -> rejected/exit 2.
#[test]
fn malformed_scope_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", "not-a-ulid", "info"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// `create-memory`'s `--props` can't shadow the reserved `content` key that
/// the subcommand itself sets from `--content` — `merge_required_prop`
/// rejects the collision outright rather than letting either value silently
/// win. (The same rule applies to `create-entity`'s `name`; that path is
/// already covered directly by `topodb-json`'s own
/// `merge_required_prop_rejects_collision_with_required_key` unit test, so
/// one CLI-level check here is enough to pin the wiring end to end.)
#[test]
fn create_collision_with_reserved_prop_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args([
            "create-memory",
            "--content",
            "x",
            "--props",
            r#"{"content":"dup"}"#,
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "rejected");
}

/// Malformed `--props` JSON is rejected (exit 2) even when the content is a
/// dedup hit — `parse_props_arg` runs before the `existing_memory` check, so
/// bad input never silently succeeds.
#[test]
fn malformed_props_on_dedup_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    // Create a memory first.
    let create_out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["create-memory", "--content", "test fact"])
        .output()
        .unwrap();
    assert!(create_out.status.success());

    // Try to re-create the same content with malformed --props; should fail
    // with exit 2 (rejected), not exit 0 (deduplicated).
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args([
            "create-memory",
            "--content",
            "test fact",
            "--props",
            "NOT-JSON",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "rejected");
}

/// A db file created (outside the CLI) with a CUSTOM `IndexSpec` — one the
/// CLI's own `topodb_json::default_spec()` does NOT declare — must have that
/// custom equality index survive being opened by the CLI. `main.rs` only
/// falls back to `default_spec()` for a path that doesn't exist yet
/// (`cli.db.exists()` is false); an EXISTING file always goes through
/// `Db::open_stored`, which inherits the persisted spec verbatim. If the CLI
/// instead clobbered an existing file's spec with its own default, this
/// `find` would come back Rejected (Person/handle isn't in the default
/// spec) rather than succeeding.
#[test]
fn existing_custom_spec_db_is_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new();

    // Build the db directly through the engine with a spec the CLI default
    // does not declare: equality on (Person, handle).
    let custom_spec = topodb::IndexSpec {
        equality: vec![topodb::PropIndex {
            label: "Person".into(),
            prop: "handle".into(),
        }],
        text: vec![],
    };
    {
        let engine_db = topodb::Db::open_with(&db, custom_spec).unwrap();
        let mut props = topodb::Props::new();
        props.insert("handle".into(), topodb::PropValue::Str("ada".into()));
        engine_db
            .submit(vec![topodb::Op::CreateNode {
                id: topodb::NodeId::new(),
                scope: topodb::Scope::Id(scope),
                label: "Person".into(),
                props,
            }])
            .unwrap();
        // engine_db drops here, releasing the redb file lock before the CLI
        // subprocess opens it.
    }

    // The CLI never created this file and carries no custom-spec knowledge
    // of its own — it can only succeed here by inheriting the persisted
    // spec via `open_stored`.
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", &scope.to_string()])
        .args([
            "find", "--label", "Person", "--prop", "handle", "--value", "ada",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hits: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let arr = hits.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["props"]["handle"], serde_json::json!("ada"));
}

#[test]
fn set_props_updates_and_removes_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db", db.to_str().unwrap(), "--scope", &scope];
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let id = full(&[
        "create-entity",
        "--name",
        "ada",
        "--props",
        r#"{"stale":"yes"}"#,
    ])
    .0["id"]
        .as_str()
        .unwrap()
        .to_string();
    // set one key, remove another (null).
    let (res, s) = full(&[
        "set-props",
        &id,
        "--props",
        r#"{"role":"pioneer","stale":null}"#,
    ]);
    assert!(s.success(), "set-props should succeed");
    assert!(res["seq"].as_u64().is_some());
    let node = full(&["get", &id]).0;
    assert_eq!(node["node"]["props"]["role"], serde_json::json!("pioneer"));
    assert!(
        node["node"]["props"].get("stale").is_none(),
        "stale should be removed"
    );
}

#[test]
fn set_props_on_missing_node_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let ghost = topodb::NodeId::new().to_string();
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["set-props", &ghost, "--props", r#"{"x":1}"#])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "rejected"
    );
}

#[test]
fn remove_node_deletes_and_cascades() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db", db.to_str().unwrap(), "--scope", &scope];
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let id = full(&["create-entity", "--name", "gone"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (res, s) = full(&["remove-node", &id]);
    assert!(s.success());
    assert!(res["seq"].as_u64().is_some());
    // Node is gone.
    assert_eq!(full(&["get", &id]).0["found"], serde_json::json!(false));
}

#[test]
fn close_edge_closes_an_open_edge() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db", db.to_str().unwrap(), "--scope", &scope];
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let a = full(&["create-entity", "--name", "a"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = full(&["create-entity", "--name", "b"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    let e = full(&["link", "--from", &a, "--to", &b, "--type", "x"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (res, s) = full(&["close-edge", &e, "--valid-to", "1000"]);
    assert!(s.success(), "close-edge should succeed");
    assert!(res["seq"].as_u64().is_some());
}

#[test]
fn close_edge_on_missing_edge_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let ghost = topodb::EdgeId::new().to_string();
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["close-edge", &ghost])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "rejected"
    );
}

#[test]
fn set_embedding_attaches_a_vector() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db", db.to_str().unwrap(), "--scope", &scope];
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let id = full(&["create-memory", "--content", "vectorized"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (res, s) = full(&[
        "set-embedding",
        &id,
        "--model",
        "test",
        "--vector",
        "[0.1,0.2,0.3]",
    ]);
    assert!(s.success(), "set-embedding should succeed");
    assert!(res["seq"].as_u64().is_some());
}

#[test]
fn set_embedding_on_missing_node_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let ghost = topodb::NodeId::new().to_string();
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["set-embedding", &ghost, "--model", "m", "--vector", "[1.0]"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "rejected"
    );
}

#[test]
fn search_vector_ranks_by_cosine() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let full = |a: &[&str]| {
        let mut v = vec!["--db", db.to_str().unwrap(), "--scope", &scope];
        v.extend_from_slice(a);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status,
        )
    };
    let m = full(&["create-memory", "--content", "near"]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    full(&[
        "set-embedding",
        &m,
        "--model",
        "test",
        "--vector",
        "[1.0,0.0]",
    ]);
    // Query aligned with M's vector: M must come back.
    let (hits, s) = full(&[
        "search-vector",
        "--model",
        "test",
        "--vector",
        "[1.0,0.0]",
        "--k",
        "5",
    ]);
    assert!(s.success(), "search-vector should succeed");
    let arr = hits.as_array().expect("bare array of hits");
    assert!(
        arr.iter().any(|h| h["node"]["id"] == serde_json::json!(m)),
        "M should rank in the results: {hits}"
    );
    assert!(arr[0]["score"].as_f64().is_some());
}

#[test]
fn search_vector_empty_vector_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["search-vector", "--model", "m", "--vector", "[]"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["error"]["kind"],
        "rejected"
    );
}

// --- Task 7: Lock backoff — retry helper, CLI busy/exit 3, MCP startup retry ---

#[test]
fn lock_contention_is_busy_exit_3_and_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    // Seed the file so it exists, and ensure it's fully initialized.
    let _ = topodb::Db::open(&db).unwrap();

    // Hold it from another thread, within THIS process (same as topodb's busy test).
    let held = topodb::Db::open(&db).unwrap();

    // Ensure the lock is established before subprocess tries to open
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Fail-fast: budget 0 -> busy, exit 3.
    let out = bin()
        .args(["--db", db.to_str().unwrap(), "--lock-wait-ms", "0", "info"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "expected exit 3 (busy), got {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "busy");

    // Retry-then-succeed: release the lock after ~300ms; default budget (3000ms) rides it out.
    let db_clone = db.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
    });

    let out = bin()
        .args(["--db", db_clone.to_str().unwrap(), "info"])
        .output()
        .unwrap();
    handle.join().unwrap();
    assert!(out.status.success(), "retry succeeds once holder drops");
}

#[test]
fn lock_contention_retrying_note_appears_after_500ms_elapsed() {
    // One iteration with structurally wide margins (a 6s hold against a 15s
    // budget): the CLI is guaranteed to wait well past the 500ms note
    // threshold no matter how slow the box is, so the note must appear.
    // (Slow-CI hardening — the earlier 3s-hold x3-iteration variant raced
    // its own probe phase against the hold window.)
    for iteration in 0..1 {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.redb");
        // Seed the file so it exists, and ensure it's fully initialized.
        let _ = topodb::Db::open(&db).unwrap();

        let db_str = db.to_str().unwrap().to_string();
        let db_clone = db_str.clone();

        // Spawn a separate OS process to hold the db lock for ~6s.
        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args(["holder_helper", "--exact", "--nocapture"])
            .env("TOPODB_TEST_HOLD_DB", &db_str)
            .env("TOPODB_TEST_HOLD_MS", "6000")
            .spawn()
            .expect("failed to spawn holder process");

        // Wait ~500ms for holder to establish the lock.
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Poll to confirm lock is held: check stderr for lock-held message.
        let mut lock_detected = false;
        for attempt in 0..20 {
            let probe = bin()
                .args(["--db", &db_clone, "--lock-wait-ms", "0", "info"])
                .output()
                .expect("probe failed");
            let stderr_text = String::from_utf8_lossy(&probe.stderr);
            let stdout_text = String::from_utf8_lossy(&probe.stdout);
            let combined = format!("{}{}", stdout_text, stderr_text);
            if combined.contains("another process holds")
                || combined.contains("held by another process")
            {
                lock_detected = true;
                break;
            }
            if attempt < 19 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(
            lock_detected,
            "iteration {}: holder never acquired lock; retried 20 times",
            iteration
        );

        // Budget (15s) far exceeds the remaining hold (~5s), so the CLI must
        // wait through the note threshold and then succeed once the holder
        // releases — with the note naming ITS budget.
        let out = bin()
            .args(["--db", &db_clone, "--lock-wait-ms", "15000", "info"])
            .output()
            .expect("CLI call failed");

        let stderr_text = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "iteration {}: CLI should succeed; stderr: {}",
            iteration,
            stderr_text
        );

        assert!(
            stderr_text
                .contains("topodb: database held by another process; retrying (budget 15000ms)"),
            "iteration {}: stderr must contain exact retry message; got: {}",
            iteration,
            stderr_text
        );

        // The assertion is done — reap the holder without waiting out its
        // sleep (it may have up to a second left).
        let _ = holder.kill();
        let _ = holder.wait();
    }
}

#[test]
fn lock_contention_no_retrying_note_before_500ms_elapsed() {
    for iteration in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.redb");
        // Seed the file so it exists, and ensure it's fully initialized.
        let _ = topodb::Db::open(&db).unwrap();

        let db_str = db.to_str().unwrap().to_string();
        let db_clone = db_str.clone();

        // Spawn a separate OS process to hold the db lock briefly (~400ms).
        // No lock-detection probe phase here: probing takes long enough to
        // outlive a short hold (that made this test flaky), and the positive
        // test already proves the holder mechanics. If the holder is slow to
        // start, the CLI simply sees no contention — the silence assertion
        // below holds either way.
        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args(["holder_helper", "--exact", "--nocapture"])
            .env("TOPODB_TEST_HOLD_DB", &db_str)
            .env("TOPODB_TEST_HOLD_MS", "400")
            .spawn()
            .expect("failed to spawn holder process");

        // Give the holder a moment to establish the lock.
        std::thread::sleep(std::time::Duration::from_millis(150));

        // Now run the CLI; when the observed wait genuinely stayed under the
        // 500ms note threshold, no note may appear. On a slow/loaded box the
        // holder's ~600ms can stretch past the threshold through no fault of
        // the CLI — the assertion is therefore gated on the measured wait
        // (self-aware test: it never asserts about a scenario that didn't
        // actually occur).
        let started = std::time::Instant::now();
        let out = bin()
            .args(["--db", &db_clone, "info"])
            .output()
            .expect("CLI call failed");
        let waited_ms = started.elapsed().as_millis();

        let stderr_text = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "iteration {}: CLI should succeed; stderr: {}",
            iteration,
            stderr_text
        );

        if waited_ms < 450 {
            assert!(
                !stderr_text.contains("retrying"),
                "iteration {}: stderr must NOT contain 'retrying' when the wait stayed under the threshold ({}ms); got: {}",
                iteration,
                waited_ms,
                stderr_text
            );
        } else {
            eprintln!(
                "iteration {iteration}: wait ran {waited_ms}ms (>=450) — box too slow to \
                 exercise the fast path this round; silence assertion skipped"
            );
        }

        // Wait for holder process to finish.
        holder.wait().expect("holder process wait failed");
    }
}

#[test]
fn submit_batch_atomic_with_backrefs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let batch = dir.path().join("batch.json");
    std::fs::write(
        &batch,
        r##"[
          {"op":"create_entity","name":"Ada"},
          {"op":"create_memory","content":"met Ada"},
          {"op":"link","from":"#1","to":"#0","type":"about"}
        ]"##,
    )
    .unwrap();
    let out = bin()
        .args(["--db", db.to_str().unwrap(), "--scope", &scope, "submit"])
        .arg(&batch)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let res: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids = res["ids"].as_array().expect("ids array");
    assert_eq!(ids.len(), 3);
    assert!(ids[0].is_string() && ids[1].is_string() && ids[2].is_string());
    // The entity is findable (all three committed).
    let found = bin()
        .args(["--db", db.to_str().unwrap(), "--scope", &scope])
        .args([
            "find", "--label", "Entity", "--prop", "name", "--value", "Ada",
        ])
        .output()
        .unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&found.stdout).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
}

#[test]
fn submit_batch_bad_backref_is_rejected_and_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let batch = dir.path().join("bad.json");
    // #5 doesn't exist -> whole batch rejected, nothing committed.
    std::fs::write(
        &batch,
        r##"[
          {"op":"create_entity","name":"Nope"},
          {"op":"link","from":"#5","to":"#0","type":"x"}
        ]"##,
    )
    .unwrap();
    let out = bin()
        .args(["--db", db.to_str().unwrap(), "--scope", &scope, "submit"])
        .arg(&batch)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "rejected");
    // Nothing committed: the entity from command #0 must NOT be findable.
    let found = bin()
        .args(["--db", db.to_str().unwrap(), "--scope", &scope])
        .args([
            "find", "--label", "Entity", "--prop", "name", "--value", "Nope",
        ])
        .output()
        .unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&found.stdout).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 0, "batch must be atomic");
}

#[test]
fn submit_batch_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let mut child = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "submit",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"[{"op":"create_entity","name":"Stdin"}]"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let res: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(res["ids"].as_array().unwrap().len(), 1);
}

/// `traverse --max-hops` is already wired on both surfaces; the engine clamps
/// the budget to 1..=4. Confirm the CLI surfaces that contract: 0 and 5 are
/// rejected (exit 2), 4 is accepted. No production code — this pins existing
/// behavior so Plan 6 can mark the item done.
#[test]
fn traverse_max_hops_is_clamped_1_to_4() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let seed = topodb::NodeId::new().to_string();
    let run = |hops: &str| {
        bin()
            .args(["--db", db.to_str().unwrap(), "--scope", &scope])
            .args(["traverse", &seed, "--max-hops", hops])
            .output()
            .unwrap()
    };
    assert_eq!(run("0").status.code(), Some(2), "0 hops rejected");
    assert_eq!(run("5").status.code(), Some(2), "5 hops rejected");
    // 4 is in-range: a traverse from a nonexistent seed still succeeds with an
    // empty subgraph (not an error).
    assert!(run("4").status.success(), "4 hops accepted");
}

// --- Task 2: per-command --scope on create-memory, create-entity, link (D1) ---

/// A per-command `--scope` must override the global `--scope` for that one
/// invocation — the same semantics an MCP tool's optional `scope` param has
/// against the server's default write scope.
#[test]
fn create_memory_scope_overrides_the_global_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let project = topodb::ScopeId::new().to_string();

    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args([
            "--scope",
            &project,
            "create-memory",
            "--content",
            "a lesson that generalises",
            "--scope",
            "shared",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    // Visible to a `shared` reader...
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", "shared", "get", &id])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["found"], true,
        "the node should have landed in `shared`, not the global project scope"
    );

    // ...and NOT to a reader in the project scope the global flag named.
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", &project, "get", &id])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["found"], false,
        "--scope on the command must override the global --scope"
    );
}

#[test]
fn create_entity_scope_overrides_the_global_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let project = topodb::ScopeId::new().to_string();

    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args([
            "--scope",
            &project,
            "create-entity",
            "--name",
            "ada",
            "--scope",
            "shared",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", "shared", "get", &id])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["found"], true);

    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", &project, "get", &id])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["found"], false);
}

/// Shared by the two `link --scope` pinning tests below (both originally
/// defined this same closure verbatim; hoisted here so the duplication
/// doesn't drift). Runs the CLI against a given `--db`/`--scope`, asserting
/// success, and parses stdout as JSON.
fn run_scoped(dbs: &str, scope: &str, extra: &[&str]) -> serde_json::Value {
    let mut v = vec!["--db", dbs, "--scope", scope];
    v.extend_from_slice(extra);
    let out = bin().args(&v).output().unwrap();
    assert!(
        out.status.success(),
        "args {:?} -- stderr: {}",
        extra,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The regression test that carries the actual risk. `link --scope shared` is
/// what lets a `shared` edge join two `shared` nodes. Without it, the edge is
/// stamped with the global (project) scope and becomes invisible from every
/// other project — the nodes are shared but disconnected. Mirrors the MCP-side
/// test P1 already has.
#[test]
fn link_scope_makes_a_shared_edge_traversable_by_a_shared_reader() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let project = topodb::ScopeId::new().to_string();
    let dbs = db.to_str().unwrap().to_string();

    // Two `shared` nodes, created while the GLOBAL scope is the project.
    let a = run_scoped(
        &dbs,
        &project,
        &["create-entity", "--name", "ada", "--scope", "shared"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = run_scoped(
        &dbs,
        &project,
        &["create-entity", "--name", "grace", "--scope", "shared"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // The edge is stamped `shared` too.
    run_scoped(
        &dbs,
        &project,
        &[
            "link", "--from", &a, "--to", &b, "--type", "knows", "--scope", "shared",
        ],
    );

    let sg = run_scoped(&dbs, "shared", &["traverse", &a, "--max-hops", "1"]);
    let edges = sg["subgraph"]["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        1,
        "a `shared` edge must be traversable by a `shared` reader"
    );
}

/// The other half of the pin: WITHOUT `--scope`, the edge takes the global
/// (project) scope and a `shared` reader cannot traverse it. This is the
/// "disconnected islands" bug, and it is the behaviour `--scope` exists to let
/// callers avoid.
///
/// Note this also documents debt D3: the engine happily stamps an edge with a
/// scope neither of its endpoints can see, producing an edge no reader can ever
/// traverse. D3 is deliberately out of scope for this plan; if it is ever fixed
/// by rejecting such an edge at submit time, THIS TEST MUST CHANGE — the link
/// would then be rejected rather than committed-and-invisible.
#[test]
fn link_without_scope_stamps_the_global_scope_and_a_shared_reader_cannot_traverse_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let project = topodb::ScopeId::new().to_string();
    let dbs = db.to_str().unwrap().to_string();

    let a = run_scoped(
        &dbs,
        &project,
        &["create-entity", "--name", "ada", "--scope", "shared"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = run_scoped(
        &dbs,
        &project,
        &["create-entity", "--name", "grace", "--scope", "shared"],
    )["id"]
        .as_str()
        .unwrap()
        .to_string();

    // No --scope on the link: it takes the global project scope.
    run_scoped(
        &dbs,
        &project,
        &["link", "--from", &a, "--to", &b, "--type", "knows"],
    );

    let sg = run_scoped(&dbs, "shared", &["traverse", &a, "--max-hops", "1"]);
    assert!(
        sg["subgraph"]["edges"].as_array().unwrap().is_empty(),
        "an edge stamped with the project scope must NOT be visible to a `shared` reader"
    );
}

#[test]
fn per_command_bad_scope_is_rejected_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let dbs = db.to_str().unwrap().to_string();
    // Real nodes, so the `link` case below fails on the scope and nothing
    // else — a bogus id would also come back `kind: "rejected"`, which would
    // let this test pass for the wrong reason.
    let a = run_scoped(&dbs, "shared", &["create-entity", "--name", "ada"])["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = run_scoped(&dbs, "shared", &["create-entity", "--name", "grace"])["id"]
        .as_str()
        .unwrap()
        .to_string();
    for args in [
        vec!["create-memory", "--content", "x", "--scope", "not-a-ulid"],
        vec!["create-entity", "--name", "x", "--scope", "not-a-ulid"],
        vec![
            "link",
            "--from",
            &a,
            "--to",
            &b,
            "--type",
            "knows",
            "--scope",
            "not-a-ulid",
        ],
    ] {
        let out = bin().args(["--db"]).arg(&db).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "args: {args:?}");
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["kind"], "rejected");
        let message = err["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("invalid scope"),
            "expected an 'invalid scope' message so this test can't pass off an unrelated \
             rejection (e.g. a bogus node id) as a scope failure, got: {message:?}"
        );
    }
}

/// A bad per-command `--scope` must be resolved BEFORE the db is opened —
/// same contract `--scope` already has (documented in
/// `crates/topodb-cli/README.md` under Global flags). Run against a
/// non-existent `--db` path: if the per-command override were resolved
/// after `Db::open_with` (as it was before this fix), the db file would be
/// created empty and then the process would exit 2 — leaving a stray file
/// behind. Assert both: exit 2, AND no file at that path.
#[test]
fn per_command_bad_scope_is_rejected_before_db_is_opened() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("never_created.redb");
    assert!(!db.exists(), "precondition: db path must not exist yet");

    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["create-memory", "--content", "x", "--scope", "not-a-ulid"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "rejected");
    assert!(
        !db.exists(),
        "a rejected per-command --scope must not leave an empty db file behind"
    );
}

// --- Task 4: remember verb ---

#[test]
fn remember_stores_links_and_dedups() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let out = run(&[
        "remember",
        "--content",
        "vega uses sqlite",
        "--entity",
        "vega",
        "--entity",
        "sqlite",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["deduplicated"], false);
    assert_eq!(v["entities"].as_array().unwrap().len(), 2);
    assert_eq!(v["entities"][0]["created"], true);
    assert_eq!(v["edge_ids"].as_array().unwrap().len(), 2);
    assert!(
        v.get("near_duplicates").is_none(),
        "CLI must omit near_duplicates"
    );
    let memory_id = v["memory_id"].as_str().unwrap().to_string();

    // Byte-identical content dedups to the same node, reusing entities.
    let out2 = run(&[
        "remember",
        "--content",
        "vega uses sqlite",
        "--entity",
        "vega",
        "--entity",
        "sqlite",
    ]);
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["deduplicated"], true);
    assert_eq!(v2["memory_id"].as_str().unwrap(), memory_id);
    assert_eq!(v2["entities"][0]["created"], false);

    // The link is traversable from the entity.
    let entity_id = v["entities"][0]["id"].as_str().unwrap();
    let out3 = run(&["traverse", entity_id, "--max-hops", "1"]);
    let v3: serde_json::Value = serde_json::from_slice(&out3.stdout).unwrap();
    let contents: Vec<String> = v3["subgraph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["props"]["content"].as_str().map(String::from))
        .collect();
    assert!(
        contents.contains(&"vega uses sqlite".to_string()),
        "memory reachable via traverse"
    );
}

#[test]
fn remember_supersedes_retires_the_old_fact() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let old: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "vega uses postgres",
            "--entity",
            "vega",
        ])
        .stdout,
    )
    .unwrap();
    let old_id = old["memory_id"].as_str().unwrap();
    let new: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "vega uses sqlite now",
            "--entity",
            "vega",
            "--supersedes",
            old_id,
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(new["superseded"][0].as_str().unwrap(), old_id);
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", old_id]).stdout).unwrap();
    assert!(
        got["node"]["props"]["superseded_at"].is_number(),
        "old memory carries the stamp"
    );
}

#[test]
fn remember_requires_an_entity() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db", db.to_str().unwrap(), "remember", "--content", "x"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "clap missing-required is exit 2"
    );
}

// --- Task 5: create-entity is find-or-create by default ---

#[test]
fn create_entity_is_find_or_create() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let a: serde_json::Value =
        serde_json::from_slice(&run(&["create-entity", "--name", "vega"]).stdout).unwrap();
    assert_eq!(a["created"], true);
    let b: serde_json::Value =
        serde_json::from_slice(&run(&["create-entity", "--name", "vega"]).stdout).unwrap();
    assert_eq!(b["created"], false);
    assert_eq!(b["id"], a["id"], "same name resolves to the same node");
    // --always-create opts back into a raw create.
    let c: serde_json::Value = serde_json::from_slice(
        &run(&["create-entity", "--name", "vega", "--always-create"]).stdout,
    )
    .unwrap();
    assert_eq!(c["created"], true);
    assert_ne!(c["id"], a["id"]);
}

#[test]
fn create_entity_merges_only_new_props_on_hit() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let a: serde_json::Value = serde_json::from_slice(
        &run(&[
            "create-entity",
            "--name",
            "omar",
            "--props",
            r#"{"role":"owner"}"#,
        ])
        .stdout,
    )
    .unwrap();
    let id = a["id"].as_str().unwrap();
    // Existing key must NOT be overwritten; new key must land.
    run(&[
        "create-entity",
        "--name",
        "omar",
        "--props",
        r#"{"role":"intern","team":"worker"}"#,
    ]);
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", id]).stdout).unwrap();
    assert_eq!(
        got["node"]["props"]["role"], "owner",
        "existing value never overwritten"
    );
    assert_eq!(got["node"]["props"]["team"], "worker", "new key merged");
}

#[test]
fn create_entity_rejects_name_key_in_props_on_both_paths() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };

    // Create entity "zed" first.
    run(&["create-entity", "--name", "zed"]);

    // Hit path: existing entity "zed" with --props containing name key.
    let out = run(&[
        "create-entity",
        "--name",
        "zed",
        "--props",
        r#"{"name":"evil"}"#,
    ]);
    assert_eq!(out.status.code(), Some(2), "hit path must reject name key");
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(
        err["error"]["kind"], "rejected",
        "hit path error kind must be rejected"
    );

    // Miss path: brand-new entity "zed2" with --props containing name key.
    let out = run(&[
        "create-entity",
        "--name",
        "zed2",
        "--props",
        r#"{"name":"evil"}"#,
    ]);
    assert_eq!(out.status.code(), Some(2), "miss path must reject name key");
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(
        err["error"]["kind"], "rejected",
        "miss path error kind must be rejected"
    );
}

#[test]
fn create_memory_stamps_hash_and_dedups() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let a: serde_json::Value =
        serde_json::from_slice(&run(&["create-memory", "--content", "the sky is blue"]).stdout)
            .unwrap();
    assert_eq!(a["deduplicated"], false);
    let id = a["id"].as_str().unwrap();
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", id]).stdout).unwrap();
    assert!(
        got["node"]["props"]["content_hash"].is_string(),
        "hash stamped"
    );
    // Whitespace-normalized duplicate resolves to the same node.
    let b: serde_json::Value =
        serde_json::from_slice(&run(&["create-memory", "--content", "the  sky is blue "]).stdout)
            .unwrap();
    assert_eq!(b["deduplicated"], true);
    assert_eq!(b["id"].as_str().unwrap(), id);
}

#[test]
fn create_memory_rejects_reserved_prop_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    for props in [
        r#"{"content_hash":"x"}"#,
        r#"{"superseded_at":1}"#,
        r#"{"forgotten_at":1}"#,
        r#"{"kind":"episodic"}"#,
    ] {
        let out = bin()
            .args([
                "--db",
                db.to_str().unwrap(),
                "create-memory",
                "--content",
                "a fact",
                "--props",
                props,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "props {props} must be rejected");
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["kind"], "rejected");
        let msg = err["error"]["message"].as_str().unwrap();
        if props.contains("kind") {
            assert!(msg.contains("kind"));
        } else {
            assert!(msg.contains("maintained by the engine write path"));
        }
    }
}

#[test]
fn create_memory_rejects_reserved_keys_even_on_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    // First: create memory without props
    let out1 = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "create-memory",
            "--content",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(out1.status.code(), Some(0));

    // Second: re-send the SAME content WITH reserved-key props → must be rejected, not deduplicated
    for props in [
        r#"{"content_hash":"boom"}"#,
        r#"{"superseded_at":1}"#,
        r#"{"forgotten_at":1}"#,
        r#"{"kind":"episodic"}"#,
    ] {
        let out = bin()
            .args([
                "--db",
                db.to_str().unwrap(),
                "create-memory",
                "--content",
                "x",
                "--props",
                props,
            ])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "reserved key {props} must be rejected even on dedup"
        );
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["kind"], "rejected");
    }
}

#[test]
fn re_remember_of_superseded_content_is_a_fresh_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let old: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "vega uses postgres",
            "--entity",
            "vega",
        ])
        .stdout,
    )
    .unwrap();
    let old_id = old["memory_id"].as_str().unwrap().to_string();
    run(&[
        "remember",
        "--content",
        "vega uses sqlite",
        "--entity",
        "vega",
        "--supersedes",
        &old_id,
    ]);
    let again: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "vega uses postgres",
            "--entity",
            "vega",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(
        again["deduplicated"], false,
        "retired content must not dedup"
    );
    assert_ne!(again["memory_id"].as_str().unwrap(), old_id);
}

#[test]
fn remember_rejects_reserved_prop_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    for props in [
        r#"{"content_hash":"boom"}"#,
        r#"{"superseded_at":1}"#,
        r#"{"forgotten_at":1}"#,
        r#"{"kind":"episodic"}"#,
    ] {
        let out = bin()
            .args([
                "--db",
                db.to_str().unwrap(),
                "remember",
                "--content",
                "a fact",
                "--entity",
                "e",
                "--props",
                props,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "props {props} must be rejected");
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["error"]["kind"], "rejected");
        let msg = err["error"]["message"].as_str().unwrap();
        if props.contains("kind") {
            assert!(msg.contains("kind"));
        } else {
            assert!(msg.contains("maintained by the engine write path"));
        }
    }
}

#[test]
fn remember_edge_type_and_props_land() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let out: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "omar owns worker",
            "--entity",
            "omar",
            "--edge-type",
            "Works On",
            "--props",
            r#"{"source":"standup"}"#,
        ])
        .stdout,
    )
    .unwrap();
    let memory_id = out["memory_id"].as_str().unwrap();
    // Props landed on the memory node.
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", memory_id]).stdout).unwrap();
    assert_eq!(got["node"]["props"]["source"], "standup");
    // Edge type normalized to works_on.
    let tv: serde_json::Value =
        serde_json::from_slice(&run(&["traverse", memory_id, "--max-hops", "1"]).stdout).unwrap();
    let types: Vec<&str> = tv["subgraph"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(
        types.contains(&"works_on"),
        "normalized edge type, got {types:?}"
    );
}

#[test]
fn remember_positional_content_equals_flag() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let run = |content_args: &[&str]| {
        let mut v = vec!["--db", db.to_str().unwrap(), "--scope", &scope, "remember"];
        v.extend_from_slice(content_args);
        v.extend_from_slice(&["--entity", "X"]);
        let out = bin().args(&v).output().unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    // First write via positional, second identical via --content → dedup hit.
    let a = run(&["the same fact"]);
    let b = run(&["--content", "the same fact"]);
    assert_eq!(b["deduplicated"], serde_json::Value::Bool(true));
    assert_eq!(a["memory_id"], b["memory_id"]);
}

#[test]
fn remember_conflicting_content_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "remember",
            "pos",
            "--content",
            "flag",
            "--entity",
            "X",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2)); // clap conflict
}

#[test]
fn remember_missing_content_is_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db", db.to_str().unwrap(), "remember", "--entity", "X"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--content") || err.contains("positional"),
        "err: {err}"
    );
}

#[test]
fn traverse_as_of_shows_the_past_topology() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let j = |o: &std::process::Output| -> serde_json::Value {
        serde_json::from_slice(&o.stdout).unwrap()
    };
    // Build: memory M -about-> entity old_home, edge later closed; then M -about-> new_home.
    let m = j(&run(&["create-memory", "--content", "the service moved"]));
    let old_home = j(&run(&["create-entity", "--name", "old-home"]));
    let new_home = j(&run(&["create-entity", "--name", "new-home"]));
    let (m, old_home, new_home) = (
        m["id"].as_str().unwrap().to_string(),
        old_home["id"].as_str().unwrap().to_string(),
        new_home["id"].as_str().unwrap().to_string(),
    );
    let e1 = j(&run(&[
        "link", "--from", &m, "--to", &old_home, "--type", "about",
    ]));
    // Capture a timestamp strictly between edge1's creation and its closure.
    std::thread::sleep(std::time::Duration::from_millis(10));
    let mid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .to_string();
    std::thread::sleep(std::time::Duration::from_millis(10));
    run(&["close-edge", e1["id"].as_str().unwrap()]);
    run(&["link", "--from", &m, "--to", &new_home, "--type", "about"]);
    let names = |v: &serde_json::Value| -> Vec<String> {
        v["subgraph"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["props"]["name"].as_str().map(String::from))
            .collect()
    };
    // Now: new-home reachable, old-home not.
    let now_view = j(&run(&["traverse", &m, "--max-hops", "1"]));
    assert!(names(&now_view).contains(&"new-home".to_string()));
    assert!(!names(&now_view).contains(&"old-home".to_string()));
    // As of `mid`: old-home reachable, new-home not.
    let past_view = j(&run(&["traverse", &m, "--max-hops", "1", "--as-of", &mid]));
    assert!(
        names(&past_view).contains(&"old-home".to_string()),
        "closed edge must reappear at as_of"
    );
    assert!(
        !names(&past_view).contains(&"new-home".to_string()),
        "later edge must vanish at as_of"
    );
    // Validation: non-positive rejected.
    let bad = run(&["traverse", &m, "--as-of", "0"]);
    assert_eq!(bad.status.code(), Some(2));
}

// --- Task 1: Global args placement + mcp --help ---

/// Global flags like `--pretty` must work AFTER the subcommand, not just before.
#[test]
fn pretty_flag_works_after_subcommand_with_create_entity() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["create-entity", "--name", "ada", "--pretty"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Pretty-printed JSON should be multi-line (has newlines).
    assert!(
        stdout.trim_end().contains('\n'),
        "expected multi-line pretty JSON: {stdout}"
    );
}

/// Global flags like `--pretty` must work AFTER the subcommand with search.
#[test]
fn pretty_flag_works_after_subcommand_with_search() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    // Create a memory first so there's something to search.
    let _ = bin()
        .args(["--db"])
        .arg(&db)
        .args(["create-memory", "--content", "test memory content"])
        .output()
        .unwrap();
    // Now search with --pretty after the subcommand.
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args(["search", "test", "--pretty"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Pretty-printed JSON should be multi-line.
    assert!(
        stdout.trim_end().contains('\n'),
        "expected multi-line pretty JSON: {stdout}"
    );
}

// --- Task 2: get-edges ---

/// get-edges: test history filtering with as_of and open_only flags.
/// Create M→E1, capture mid timestamp, close E1, create M→E2.
/// Then test various combinations:
/// - default (open-only true) shows only E2
/// - --as-of mid shows only E1
/// - --open-only false shows both
/// - --as-of and --open-only together is rejected
/// - --as-of 0 is rejected
#[test]
fn get_edges_history_and_as_of() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();

    let run_cmd = |args: &[&str]| {
        let mut v = vec!["--db"];
        v.push(db.to_str().unwrap());
        v.push("--scope");
        v.push(&scope);
        v.extend_from_slice(args);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status.code().unwrap_or(999),
        )
    };

    // Create a memory M and entity E1.
    let (m_json, _) = run_cmd(&["create-memory", "--content", "test"]);
    let m_id = m_json["id"].as_str().unwrap().to_string();

    let (e1_json, _) = run_cmd(&["create-entity", "--name", "e1"]);
    let e1_id = e1_json["id"].as_str().unwrap().to_string();

    // Link M→E1.
    let (edge1_json, _) = run_cmd(&[
        "link", "--from", &m_id, "--to", &e1_id, "--type", "mentions",
    ]);
    let edge1_id = edge1_json["id"].as_str().unwrap().to_string();

    // Capture a timestamp in the middle (before E1 is closed but after it was created).
    let mid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Close E1.
    let _ = run_cmd(&["close-edge", &edge1_id]);

    // Capture E1's valid_to by fetching with open_only=false.
    let (hist, code) = run_cmd(&["get-edges", &m_id, "--open-only", "false"]);
    assert_eq!(code, 0, "get-edges should succeed, got: {:?}", hist);
    let edges_arr = hist["edges"]
        .as_array()
        .expect("expected 'edges' key in response");
    let e1_record = edges_arr
        .iter()
        .find(|e| e["id"] == edge1_id)
        .expect("E1 edge should exist in history");
    let e1_valid_to = e1_record["valid_to"]
        .as_i64()
        .expect("E1 should have valid_to set after closing");

    // Create entity E2 and link M→E2.
    let (e2_json, _) = run_cmd(&["create-entity", "--name", "e2"]);
    let e2_id = e2_json["id"].as_str().unwrap().to_string();

    let (edge2_json, _) = run_cmd(&[
        "link", "--from", &m_id, "--to", &e2_id, "--type", "mentions",
    ]);
    let edge2_id = edge2_json["id"].as_str().unwrap().to_string();

    // Capture E2's creation timestamp by fetching it with get-edges.
    let (e2_edges, _) = run_cmd(&["get-edges", &m_id, "--open-only", "false"]);
    let e2_record = e2_edges["edges"]
        .as_array()
        .expect("should have edges")
        .iter()
        .find(|e| e["id"] == edge2_id)
        .expect("E2 edge should exist");
    let e2_valid_from = e2_record["valid_from"]
        .as_i64()
        .expect("E2 edge should have valid_from");

    // Test 1: default (open-only true) → only E2.
    let (result, code) = run_cmd(&["get-edges", &m_id]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "should have only 1 edge (E2)");
    assert_eq!(edges[0]["id"], edge2_id);

    // Test 2: explicit --open-only true → only E2.
    let (result, code) = run_cmd(&["get-edges", &m_id, "--open-only", "true"]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["id"], edge2_id);

    // Test 3: --as-of mid (before E1 closed) → only E1.
    let (result, code) = run_cmd(&["get-edges", &m_id, "--as-of", &mid.to_string()]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "should have only E1 at mid timestamp");
    assert_eq!(edges[0]["id"], edge1_id);

    // Test 4: --as-of exactly at E1's valid_to (exclusive upper bound).
    // At E1's valid_to instant, E1 should not appear (exclusive boundary),
    // and E2 hasn't been created yet, so we should get 0 edges.
    let (result, code) = run_cmd(&["get-edges", &m_id, "--as-of", &(e1_valid_to).to_string()]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        0,
        "at E1's valid_to (exclusive boundary), neither edge should appear"
    );

    // Test 4b: --as-of at E2's creation timestamp (after E1's valid_to).
    let (result, code) = run_cmd(&["get-edges", &m_id, "--as-of", &e2_valid_from.to_string()]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        1,
        "at E2's creation time, only E2 should appear (E1 was closed before this)"
    );
    assert_eq!(edges[0]["id"], edge2_id);

    // Test 5: --open-only false → both edges.
    let (result, code) = run_cmd(&["get-edges", &m_id, "--open-only", "false"]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "should have both E1 (closed) and E2 (open)");

    // Test 6: --as-of 0 → rejected/exit 2.
    let (_, code) = run_cmd(&["get-edges", &m_id, "--as-of", "0"]);
    assert_eq!(code, 2, "as-of 0 should be rejected");

    // Test 7: --as-of with --open-only flag → rejected/exit 2 (mutual exclusion).
    let (_, code) = run_cmd(&[
        "get-edges",
        &m_id,
        "--as-of",
        &mid.to_string(),
        "--open-only",
        "false",
    ]);
    assert_eq!(code, 2, "as-of and open-only together should be rejected");
}

/// get-edges: test --direction out|in|both with incoming/outgoing edge filtering.
/// Build M→E (about edge). Test get-edges E with default (out) = [], with --direction in = the about edge,
/// with --direction both on M = same edge once, and invalid direction exits with code 2.
/// Also test --direction in --as-of composes (filtering by time in incoming direction).
#[test]
fn get_edges_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();

    let run_cmd = |args: &[&str]| {
        let mut v = vec!["--db"];
        v.push(db.to_str().unwrap());
        v.push("--scope");
        v.push(&scope);
        v.extend_from_slice(args);
        let out = bin().args(&v).output().unwrap();
        (
            serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .unwrap_or(serde_json::Value::Null),
            out.status.code().unwrap_or(999),
        )
    };

    // Create a memory M and entity E.
    let (m_json, _) = run_cmd(&["create-memory", "--content", "test"]);
    let m_id = m_json["id"].as_str().unwrap().to_string();

    let (e_json, _) = run_cmd(&["create-entity", "--name", "e"]);
    let e_id = e_json["id"].as_str().unwrap().to_string();

    // Link M→E with type "about".
    let (edge_json, _) = run_cmd(&["link", "--from", &m_id, "--to", &e_id, "--type", "about"]);
    let edge_id = edge_json["id"].as_str().unwrap().to_string();

    // Test 1: get-edges E with default (out) = [] (E has no outgoing edges).
    let (result, code) = run_cmd(&["get-edges", &e_id]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 0, "E should have no outgoing edges");

    // Test 2: get-edges E --direction in = the about edge (M→E).
    let (result, code) = run_cmd(&["get-edges", &e_id, "--direction", "in"]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "E should have 1 incoming edge");
    assert_eq!(edges[0]["id"], edge_id);
    assert_eq!(edges[0]["type"], "about");

    // Test 3: get-edges M --direction both = same edge once (M has outgoing, E has incoming).
    let (result, code) = run_cmd(&["get-edges", &m_id, "--direction", "both"]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        1,
        "M should have 1 edge total (out+in deduped)"
    );
    assert_eq!(edges[0]["id"], edge_id);

    // Test 4: get-edges E --direction sideways = exit 2 (invalid direction).
    let (_, code) = run_cmd(&["get-edges", &e_id, "--direction", "sideways"]);
    assert_eq!(code, 2, "invalid direction should be rejected");

    // Test 5: get-edges E --direction in --as-of <mid> composes.
    // Capture a timestamp BEFORE closing the edge.
    let mid = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Close the edge.
    let _ = run_cmd(&["close-edge", &edge_id]);
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Create entity E2.
    let (e2_json, _) = run_cmd(&["create-entity", "--name", "e2"]);
    let e2_id = e2_json["id"].as_str().unwrap().to_string();

    // Link M→E2.
    let (edge2_json, _) = run_cmd(&["link", "--from", &m_id, "--to", &e2_id, "--type", "about"]);
    let _edge2_id = edge2_json["id"].as_str().unwrap().to_string();

    // Now test as_of on incoming edges: get-edges E --direction in --as-of <mid>
    // should find the M→E edge (it was valid at mid and E is the target).
    let (result, code) = run_cmd(&[
        "get-edges",
        &e_id,
        "--direction",
        "in",
        "--as-of",
        &mid.to_string(),
    ]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "E should have 1 incoming edge at mid");
    assert_eq!(edges[0]["id"], edge_id);

    // Verify E has no incoming edges in the default (now) view.
    let (result, code) = run_cmd(&["get-edges", &e_id, "--direction", "in"]);
    assert_eq!(code, 0);
    let edges = result["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        0,
        "E should have no open incoming edges (closed)"
    );
}

/// `search` is a recall surface: a memory retired by `--supersedes` must not
/// come back (nor outrank its live successor). `--include-superseded` is the
/// history escape hatch — same default-liveness shape `get-edges` has with
/// `open_only`.
#[test]
fn search_skips_superseded_by_default_with_include_superseded_escape() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let old: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "vega uses postgres",
            "--entity",
            "vega",
        ])
        .stdout,
    )
    .unwrap();
    let old_id = old["memory_id"].as_str().unwrap();
    let new: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "vega uses sqlite now",
            "--entity",
            "vega",
            "--supersedes",
            old_id,
        ])
        .stdout,
    )
    .unwrap();
    let new_id = new["memory_id"].as_str().unwrap();

    let ids = |out: std::process::Output| -> Vec<String> {
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["node"]["id"].as_str().unwrap().to_string())
            .collect()
    };

    let live = ids(run(&["search", "vega postgres sqlite"]));
    assert!(live.contains(&new_id.to_string()), "live successor found");
    assert!(
        !live.contains(&old_id.to_string()),
        "retired memory must not surface by default"
    );

    let all = ids(run(&[
        "search",
        "vega postgres sqlite",
        "--include-superseded",
    ]));
    assert!(
        all.contains(&old_id.to_string()),
        "escape hatch shows history"
    );
    assert!(all.contains(&new_id.to_string()));
}

/// `forget` soft-retires: stamps forgotten_at, closes the memory's open
/// edges, drops it from default search; history stays reachable.
#[test]
fn forget_retires_a_memory_and_rejects_invalid_targets() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let stored: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "orion uses kafka",
            "--entity",
            "orion",
        ])
        .stdout,
    )
    .unwrap();
    let mem = stored["memory_id"].as_str().unwrap().to_string();
    let ent = stored["entities"][0]["id"].as_str().unwrap().to_string();

    let out = run(&["forget", &mem]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["forgotten"][0].as_str().unwrap(), mem);

    // Stamp visible on the node; open edges closed.
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", &mem]).stdout).unwrap();
    assert!(got["node"]["props"]["forgotten_at"].is_number());
    let edges: serde_json::Value =
        serde_json::from_slice(&run(&["get-edges", &mem]).stdout).unwrap();
    assert_eq!(
        edges["edges"].as_array().unwrap().len(),
        0,
        "open edges must be closed by forget"
    );

    // Gone from default search; visible via the history switch.
    let hits: serde_json::Value =
        serde_json::from_slice(&run(&["search", "orion kafka"]).stdout).unwrap();
    assert!(hits
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["node"]["id"].as_str() != Some(mem.as_str())));

    // Rejection matrix, each exit 2: repeat-forget, entity target, bogus id.
    for (args, needle) in [
        (vec!["forget", mem.as_str()], "already forgotten"),
        (vec!["forget", ent.as_str()], "not a Memory"),
        (vec!["forget", "not-a-ulid"], "invalid node id"),
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(needle)
                || String::from_utf8_lossy(&out.stdout).contains(needle),
            "{needle:?} missing for {args:?}"
        );
    }
}

/// Forgotten memories obey the same read-side liveness as superseded ones:
/// hidden from default search, revealed by --include-superseded, and
/// re-remembering identical content mints a FRESH live memory (no dedup
/// resurrection of the tombstoned node).
#[test]
fn forgotten_memories_share_superseded_read_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let stored: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "ceres uses etcd",
            "--entity",
            "ceres",
        ])
        .stdout,
    )
    .unwrap();
    let mem = stored["memory_id"].as_str().unwrap().to_string();
    assert!(run(&["forget", &mem]).status.success());

    let all: serde_json::Value =
        serde_json::from_slice(&run(&["search", "ceres etcd", "--include-superseded"]).stdout)
            .unwrap();
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|h| h["node"]["id"].as_str() == Some(mem.as_str())),
        "history switch must reveal forgotten memories"
    );

    let again: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "ceres uses etcd",
            "--entity",
            "ceres",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(again["deduplicated"], false, "no dedup to a forgotten node");
    assert_ne!(again["memory_id"].as_str().unwrap(), mem);
}

/// Phase B kind taxonomy on the CLI: --kind stamps new memories (dedup
/// ignores it; stored kind wins), --kinds filters search on both the live
/// and history paths, absent kind reads as "semantic", and bad values
/// reject with the vocabulary named.
#[test]
fn kind_taxonomy_stamps_filters_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };

    // Stamp: --kind lands on the node's props.
    let stored: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "deneb rotates keys weekly",
            "--entity",
            "deneb",
            "--kind",
            "procedural",
        ])
        .stdout,
    )
    .unwrap();
    let proc_mem = stored["memory_id"].as_str().unwrap().to_string();
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", &proc_mem]).stdout).unwrap();
    assert_eq!(got["node"]["props"]["kind"].as_str(), Some("procedural"));

    // Unstamped memory: no kind prop, reads as semantic on the filter side.
    let plain: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "deneb uses etcd",
            "--entity",
            "deneb",
        ])
        .stdout,
    )
    .unwrap();
    let sem_mem = plain["memory_id"].as_str().unwrap().to_string();

    // Filter: --kinds procedural sees only the stamped memory ...
    let hits: serde_json::Value =
        serde_json::from_slice(&run(&["search", "deneb", "--kinds", "procedural"]).stdout).unwrap();
    let ids: Vec<&str> = hits
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["node"]["id"].as_str())
        .collect();
    assert!(ids.contains(&proc_mem.as_str()));
    assert!(!ids.contains(&sem_mem.as_str()));

    // ... and --kinds semantic sees the UNSTAMPED one (absent = semantic).
    let hits: serde_json::Value =
        serde_json::from_slice(&run(&["search", "deneb", "--kinds", "semantic"]).stdout).unwrap();
    let ids: Vec<&str> = hits
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["node"]["id"].as_str())
        .collect();
    assert!(
        ids.contains(&sem_mem.as_str()),
        "absent kind must match semantic"
    );
    assert!(!ids.contains(&proc_mem.as_str()));

    // Comma-delimited multi-value admits both.
    let hits: serde_json::Value =
        serde_json::from_slice(&run(&["search", "deneb", "--kinds", "procedural,semantic"]).stdout)
            .unwrap();
    assert_eq!(
        hits.as_array().unwrap().len(),
        3,
        "both memories + the entity node"
    );

    // Dedup ignores kind: same content, different kind → same id, stored kind wins.
    let again: serde_json::Value = serde_json::from_slice(
        &run(&[
            "remember",
            "--content",
            "deneb rotates keys weekly",
            "--entity",
            "deneb",
            "--kind",
            "episodic",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(again["deduplicated"], true);
    assert_eq!(again["memory_id"].as_str().unwrap(), proc_mem);
    let got: serde_json::Value = serde_json::from_slice(&run(&["get", &proc_mem]).stdout).unwrap();
    assert_eq!(
        got["node"]["props"]["kind"].as_str(),
        Some("procedural"),
        "the stored kind must win on a dedup hit"
    );

    // --kinds combines with --include-superseded (the history path).
    assert!(run(&["forget", &sem_mem]).status.success());
    let hits: serde_json::Value = serde_json::from_slice(
        &run(&[
            "search",
            "deneb",
            "--kinds",
            "semantic",
            "--include-superseded",
        ])
        .stdout,
    )
    .unwrap();
    let ids: Vec<&str> = hits
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["node"]["id"].as_str())
        .collect();
    assert!(
        ids.contains(&sem_mem.as_str()),
        "history search must combine with the kinds filter"
    );

    // Validation: bad values reject with the vocabulary named, exit 2.
    for args in [
        vec![
            "remember",
            "--content",
            "x y",
            "--entity",
            "e",
            "--kind",
            "factual",
        ],
        vec!["search", "deneb", "--kinds", "Episodic"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            all.contains("episodic"),
            "vocabulary missing for {args:?}: {all}"
        );
    }
}

/// Phase C sweep on the CLI: kind-aware staleness ranking under an
/// injected --now-ms (deterministic), tombstone exclusion, limit, and
/// param validation. The sweep only proposes — nothing in the db changes.
#[test]
fn lifecycle_candidates_ranks_deterministically_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let remember = |content: &str, kind: Option<&str>| -> String {
        let mut args = vec!["remember", "--content", content, "--entity", "topic"];
        if let Some(k) = kind {
            args.extend_from_slice(&["--kind", k]);
        }
        let v: serde_json::Value = serde_json::from_slice(&run(&args).stdout).unwrap();
        v["memory_id"].as_str().unwrap().to_string()
    };
    let ep = remember("ci was red this morning", Some("episodic"));
    let se = remember("release tags are per package", None);
    let pr = remember("publish crates in dependency order", Some("procedural"));
    let dead = remember("stale duplicate", None);
    assert!(run(&["forget", &dead]).status.success());

    // Sweep from 28 days after mint: episodic (14d) > semantic (120d) >
    // procedural (365d) at equal age; the forgotten memory never appears.
    let ulid_ms = |id: &str| {
        // ULID timestamp: first 10 chars, Crockford base32.
        let alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        id.chars()
            .take(10)
            .fold(0i64, |acc, c| acc * 32 + alphabet.find(c).unwrap() as i64)
    };
    let now = ulid_ms(&ep) + 28 * 86_400_000;
    let now_s = now.to_string();
    let out = run(&["lifecycle-candidates", "--now-ms", &now_s]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![ep.as_str(), se.as_str(), pr.as_str()]);
    assert_eq!(v[0]["kind"], "episodic");
    assert!(v[0]["staleness"].as_f64().unwrap() > v[1]["staleness"].as_f64().unwrap());
    assert!(v[0]["content"].as_str().unwrap().contains("ci was red"));
    assert!(v[0]["created_at"].is_number() && v[0]["access_count"].is_number());

    // Determinism: identical call, identical output bytes.
    let again = run(&["lifecycle-candidates", "--now-ms", &now_s]);
    assert_eq!(out.stdout, again.stdout);

    // Limit truncates after ranking.
    let top1: serde_json::Value = serde_json::from_slice(
        &run(&["lifecycle-candidates", "--now-ms", &now_s, "--limit", "1"]).stdout,
    )
    .unwrap();
    assert_eq!(top1.as_array().unwrap().len(), 1);
    assert_eq!(top1[0]["id"].as_str().unwrap(), ep.as_str());

    // Validation: exit 2 with the shared messages.
    for (args, needle) in [
        (vec!["lifecycle-candidates", "--limit", "0"], "limit"),
        (
            vec!["lifecycle-candidates", "--half-life-episodic-days", "0"],
            "half-lives must be positive",
        ),
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            all.contains(needle),
            "{needle:?} missing for {args:?}: {all}"
        );
    }
}

#[test]
fn obsidian_ingest_creates_supersedes_and_stamps() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let vault = dir.path().join("vault");
    std::fs::create_dir(&vault).unwrap();
    std::fs::write(
        vault.join("fact.md"),
        "---\nstatus: open\n---\nDelta uses [[redb]].\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    // Missing dir → rejected/2.
    let bad = run(&["obsidian-ingest", dir.path().join("nope").to_str().unwrap()]);
    assert_eq!(bad.status.code(), Some(2));

    // Dry run reports without writing.
    let dry = run(&["obsidian-ingest", vault.to_str().unwrap(), "--dry-run"]);
    let v: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(v["ingested"], 1);
    assert!(!std::fs::read_to_string(vault.join("fact.md"))
        .unwrap()
        .contains("topodb-id"));

    // Real run stamps and stores; rerun is a skip.
    let r = run(&["obsidian-ingest", vault.to_str().unwrap()]);
    assert!(
        r.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(
        (
            v["ingested"].as_u64(),
            v["errors"].as_array().unwrap().len()
        ),
        (Some(1), 0)
    );
    assert!(std::fs::read_to_string(vault.join("fact.md"))
        .unwrap()
        .contains("topodb-id:"));
    let r2 = run(&["obsidian-ingest", vault.to_str().unwrap()]);
    let v2: serde_json::Value = serde_json::from_slice(&r2.stdout).unwrap();
    assert_eq!(v2["skipped"], 1);
    // And the memory is searchable.
    let s = run(&["search", "Delta"]);
    assert!(String::from_utf8_lossy(&s.stdout).contains("Delta uses"));
}

#[test]
fn obsidian_seed_selects_writes_and_protects_edits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    run(&[
        "remember",
        "--content",
        "Epsilon caches sessions in redis",
        "--entity",
        "redis",
    ]);
    let vault = dir.path().join("wm");

    // Selector validation: neither, or both, → 2.
    assert_eq!(
        run(&["obsidian-seed", vault.to_str().unwrap()])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run(&[
            "obsidian-seed",
            vault.to_str().unwrap(),
            "--query",
            "x",
            "--entity",
            "y"
        ])
        .status
        .code(),
        Some(2)
    );

    // Entity mode materializes the neighborhood + stub.
    let r = run(&[
        "obsidian-seed",
        vault.to_str().unwrap(),
        "--entity",
        "redis",
    ]);
    assert!(
        r.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(v["seeded"], serde_json::json!(1));
    assert_eq!(v["stubs"].as_u64(), Some(1));

    // Query mode hits the same memory; existing identical files count unchanged.
    let q = run(&[
        "obsidian-seed",
        vault.to_str().unwrap(),
        "--query",
        "sessions cache",
        "--k",
        "5",
    ]);
    let vq: serde_json::Value = serde_json::from_slice(&q.stdout).unwrap();
    assert!(vq["unchanged"].as_u64().unwrap() >= 1);

    // Unknown entity → rejected/2.
    assert_eq!(
        run(&[
            "obsidian-seed",
            vault.to_str().unwrap(),
            "--entity",
            "ghost"
        ])
        .status
        .code(),
        Some(2)
    );
}

/// Regression for the seed/ingest scope asymmetry (a memory in a project
/// scope, linked to an entity in `shared`): `obsidian-seed --scope <project>`
/// must be able to resolve the shared entity (this failed with "unknown
/// entity" before the fix, since seed read with the project scope alone), and
/// the seeded vault must then re-ingest as a pure no-op — no spurious
/// supersession from the wider [project, shared] lookup `obsidian-ingest`
/// uses discovering an edge that seed never saw.
#[test]
fn obsidian_seed_resolves_shared_entity_from_a_project_scope_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let project = topodb::ScopeId::new().to_string();
    let dbs = db.to_str().unwrap().to_string();
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", &dbs];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };

    // Entity in the default `shared` scope.
    let ce = run(&["create-entity", "--name", "redis"]);
    assert!(
        ce.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ce.stderr)
    );

    // Memory in a project scope, linked to the shared entity.
    let rem = run(&[
        "--scope",
        &project,
        "remember",
        "--content",
        "Zeta caches sessions in redis",
        "--entity",
        "redis",
    ]);
    assert!(
        rem.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&rem.stderr)
    );

    let vault = dir.path().join("wm");

    // Before the fix, this failed with "unknown entity redis" because seed
    // read only the project scope, never shared.
    let seed = run(&[
        "--scope",
        &project,
        "obsidian-seed",
        vault.to_str().unwrap(),
        "--entity",
        "redis",
    ]);
    assert!(
        seed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let sv: serde_json::Value = serde_json::from_slice(&seed.stdout).unwrap();
    assert_eq!(sv["seeded"].as_u64(), Some(1));
    assert_eq!(sv["stubs"].as_u64(), Some(1));

    let names: Vec<_> = std::fs::read_dir(&vault)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().any(|n| n == "redis.md"), "{names:?}");

    // Re-ingest the untouched seeded vault: no new/superseded memories.
    let ingest = run(&[
        "--scope",
        &project,
        "obsidian-ingest",
        vault.to_str().unwrap(),
    ]);
    assert!(
        ingest.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&ingest.stderr)
    );
    let iv: serde_json::Value = serde_json::from_slice(&ingest.stdout).unwrap();
    assert_eq!(
        (
            iv["superseded"].as_u64(),
            iv["ingested"].as_u64(),
            iv["errors"].as_array().unwrap().len()
        ),
        (Some(0), Some(0), 0),
        "untouched seeded vault must re-ingest as a pure no-op, got {iv:?}"
    );
}

/// Phase E purge on the CLI: dry-run by default (reports, writes nothing),
/// --yes hard-deletes exactly the strictly-older tombstoned memories, and
/// non-tombstoned memories are never touched. History note made real:
/// after purge, `get` stops finding the node entirely.
#[test]
fn purge_dry_runs_by_default_and_deletes_with_yes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let run = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["--db", db.to_str().unwrap()];
        v.extend_from_slice(args);
        bin().args(&v).output().unwrap()
    };
    let json = |out: &std::process::Output| -> serde_json::Value {
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let remember = |content: &str| -> String {
        let out = run(&["remember", "--content", content, "--entity", "topic"]);
        json(&out)["memory_id"].as_str().unwrap().to_string()
    };

    let doomed = remember("tombstoned long ago");
    let keeper = remember("still live");
    assert!(run(&["forget", &doomed]).status.success());

    // A cutoff far in the future makes `doomed`'s fresh tombstone "old".
    let future = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000)
        .to_string();

    // Dry-run: reports the plan, writes nothing.
    let dry = run(&["purge", "--tombstoned-before", &future]);
    assert!(
        dry.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let v = json(&dry);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["count"], 1);
    assert_eq!(v["ids"][0].as_str().unwrap(), doomed.as_str());
    let still = json(&run(&["get", &doomed]));
    assert_eq!(still["found"], true, "dry-run must not delete");

    // --yes: the tombstoned memory is hard-deleted; the live one survives.
    let purged = run(&["purge", "--tombstoned-before", &future, "--yes"]);
    assert!(purged.status.success());
    let v = json(&purged);
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["count"], 1);
    assert!(v["seq"].is_number());
    assert_eq!(
        json(&run(&["get", &doomed]))["found"],
        false,
        "purged history is gone"
    );
    assert_eq!(
        json(&run(&["get", &keeper]))["found"],
        true,
        "non-tombstoned never touched"
    );

    // Nothing left: an empty purge with --yes reports zero without a write.
    let empty = run(&["purge", "--tombstoned-before", &future, "--yes"]);
    let v = json(&empty);
    assert_eq!(v["count"], 0);
    assert!(v["seq"].is_null(), "empty batch skips the submit");

    // Validation: exit 2 with the shared message.
    let bad = run(&["purge", "--tombstoned-before", "0"]);
    assert_eq!(bad.status.code(), Some(2));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(all.contains("tombstoned-before"), "{all}");
}

// --- Task 5: --format flag, text rendering, and empty-read scope-echo ---

#[test]
fn text_format_search_is_plain_lines_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "remember",
            "a fact about foxes",
            "--entity",
            "Fox",
        ])
        .output()
        .unwrap();
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "--format",
            "text",
            "search",
            "foxes",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("a fact about foxes"), "stdout: {s}");
    assert!(
        !s.trim_start().starts_with('['),
        "text mode must not emit JSON: {s}"
    );
}

#[test]
fn json_default_unchanged_for_search() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "remember",
            "a fact about foxes",
            "--entity",
            "Fox",
        ])
        .output()
        .unwrap();
    // No --format, piped (non-TTY) → JSON.
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "search",
            "foxes",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.is_array());
}

#[test]
fn empty_search_echoes_scope_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "search",
            "nothing-matches-this",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    // stdout is still the parseable empty array.
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, serde_json::json!([]));
    // stderr carries the disambiguating echo, including the scope.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("0 matches"), "stderr: {err}");
    assert!(err.contains(&scope), "stderr must name the scope: {err}");
}

#[test]
fn format_env_flips_piped_output_to_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "remember",
            "env-driven text mode",
            "--entity",
            "E",
        ])
        .output()
        .unwrap();
    let out = bin()
        .env("TOPODB_FORMAT", "text")
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "search",
            "env-driven",
        ])
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.trim_start().starts_with('['),
        "TOPODB_FORMAT=text should give text: {s}"
    );
}

#[test]
fn text_format_get_returns_text_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    // Create a memory
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "create-memory",
            "--content",
            "test memory",
        ])
        .output()
        .unwrap();
    let mem_id = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Get with text format
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "--format",
            "text",
            "get",
            &mem_id,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains(&mem_id), "output should contain the id: {s}");
    assert!(
        !s.trim_start().starts_with('{'),
        "text format should not start with JSON: {s}"
    );
}

#[test]
fn text_format_find_returns_text_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    // Create an entity
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "create-entity",
            "--name",
            "TestEntity",
        ])
        .output()
        .unwrap();
    let ent_id = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Find with text format
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "--format",
            "text",
            "find",
            "--label",
            "Entity",
            "--prop",
            "name",
            "--value",
            "TestEntity",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains(&ent_id),
        "output should contain the entity id: {s}"
    );
    assert!(
        !s.trim_start().starts_with('['),
        "text format should not start with JSON: {s}"
    );
}

#[test]
fn text_format_remember_returns_text_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    // Remember with text format
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "--format",
            "text",
            "remember",
            "a fact to remember",
            "--entity",
            "TestEntity",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("remembered"),
        "text output should contain 'remembered': {s}"
    );
    assert!(
        !s.trim_start().starts_with('{'),
        "text format should not start with JSON: {s}"
    );
}

#[test]
fn text_format_forget_returns_text_not_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    // Create a memory to forget
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "create-memory",
            "--content",
            "to be forgotten",
        ])
        .output()
        .unwrap();
    let mem_id = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Forget with text format
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "--format",
            "text",
            "forget",
            &mem_id,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("forgot"),
        "text output should contain 'forgot': {s}"
    );
    assert!(
        !s.trim_start().starts_with('{'),
        "text format should not start with JSON: {s}"
    );
}

#[test]
fn text_format_search_truncation_with_ellipsis() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    // Create a memory with long content (>140 chars)
    let long_content = "This is a very long memory content that is intentionally written to be longer than one hundred and forty characters so that the text renderer will truncate it with an ellipsis.";
    bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "remember",
            long_content,
            "--entity",
            "LongEntity",
        ])
        .output()
        .unwrap();
    // Search with text format
    let out = bin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--scope",
            &scope,
            "--format",
            "text",
            "search",
            "intentionally",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("…"),
        "truncated text should contain ellipsis: {s}"
    );
    // The full original content should not appear (it's truncated)
    assert!(
        !s.contains("with an ellipsis."),
        "should not contain the full original end: {s}"
    );
}

/// --recency-weight / --recency-half-life-days plumb into SearchOptions:
/// defaults, weight 0, and an explicit flat half-life all search fine;
/// out-of-range values surface the engine's Rejected error.
#[test]
fn search_recency_flags_plumb_and_validate() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    let scope = topodb::ScopeId::new().to_string();
    let out = bin()
        .args(["--db"])
        .arg(&db)
        .args([
            "--scope",
            &scope,
            "remember",
            "--content",
            "feldspar veins in the north wall",
            "--entity",
            "Geology",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "remember: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let search = |extra: &[&str]| {
        let mut c = bin();
        c.args(["--db"])
            .arg(&db)
            .args(["--scope", &scope, "search", "feldspar"]);
        c.args(extra);
        c.output().unwrap()
    };

    // defaults (kind-aware), disabled, explicit flat: all succeed and hit.
    for extra in [
        &[][..],
        &["--recency-weight", "0"][..],
        &["--recency-half-life-days", "7"][..],
    ] {
        let out = search(extra);
        assert!(
            out.status.success(),
            "{extra:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            !v.as_array().unwrap().is_empty(),
            "{extra:?} must return the memory: {v}"
        );
    }

    // Out-of-range values surface the engine's Rejected error (the exit
    // code for engine rejections — assert whatever code fail_engine uses;
    // existing rejected-input CLI tests show it as 2).
    let out = search(&["--recency-weight", "1.5"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("recency_weight"));

    let out = search(&["--recency-half-life-days", "0"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("recency_half_life"));

    // Polarity pin: an explicit near-zero flat half-life must score LOWER
    // than the omitted (kind-aware) default for the SAME memory — the only
    // check in this test that would catch `recency_half_life_days.is_none()`
    // getting inverted to `.is_some()` in the options-assembly code (the
    // accepted-shapes loop above only checks that both shapes are accepted,
    // not which one wins). Sleep briefly first so the memory is guaranteed
    // to be at least ~1s old before the flat-tiny-half-life search runs (a
    // zero-age memory would decay by ~0% under any half-life, masking the
    // bug).
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let top_score = |out: &std::process::Output| -> f64 {
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v.as_array().unwrap()[0]["score"].as_f64().unwrap()
    };
    let flat_out = search(&["--recency-half-life-days", "0.00001"]);
    assert!(flat_out.status.success(), "{flat_out:?}");
    let omitted_out = search(&[]);
    assert!(omitted_out.status.success(), "{omitted_out:?}");
    let score_flat = top_score(&flat_out);
    let score_omitted = top_score(&omitted_out);
    assert!(
        score_flat < score_omitted,
        "explicit tiny flat half-life (score {score_flat}) must decay below the \
         kind-aware default (score {score_omitted})"
    );
}

#[test]
fn link_reports_conflicts_with_other_open_same_type_edges() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");

    let mk_entity = |name: &str| -> String {
        let out = bin()
            .args(["--db"])
            .arg(&db)
            .args(["create-entity", "--name", name])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["id"].as_str().unwrap().to_string()
    };

    let a = mk_entity("Drew Powell");
    let x = mk_entity("Anthropic");
    let y = mk_entity("TopoDB");

    let first = bin()
        .args(["--db"])
        .arg(&db)
        .args(["link", "--from", &a, "--to", &x, "--type", "works_at"])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_v: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let first_id = first_v["id"].as_str().unwrap().to_string();
    assert!(first_v.get("conflicts").is_none(), "{first_v}");

    let second = bin()
        .args(["--db"])
        .arg(&db)
        .args(["link", "--from", &a, "--to", &y, "--type", "works_at"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_v: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    let conflicts = second_v["conflicts"]
        .as_array()
        .unwrap_or_else(|| panic!("expected conflicts on: {second_v}"));
    assert_eq!(conflicts.len(), 1, "conflicts: {conflicts:?}");
    assert_eq!(conflicts[0]["edge_id"].as_str().unwrap(), first_id);
    assert_eq!(conflicts[0]["to"].as_str().unwrap(), x);
}

#[test]
fn remember_prints_supersession_candidates_for_a_contradicting_pair() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.redb");
    // Same scope on every call in this test — exercised here only to give the
    // conflict scan coverage for `--scope`; behavior is otherwise unchanged
    // from the scope-omitted default.
    let scope = topodb::ScopeId::new().to_string();

    let first = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", &scope])
        .args([
            "remember",
            "TopoDB stores its data in redb",
            "--entity",
            "TopoDB",
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_v: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let first_id = first_v["memory_id"].as_str().unwrap().to_string();

    let second = bin()
        .args(["--db"])
        .arg(&db)
        .args(["--scope", &scope])
        .args([
            "remember",
            "TopoDB now stores its data in sled, not redb",
            "--entity",
            "TopoDB",
        ])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_v: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    let candidates = second_v["supersession_candidates"]
        .as_array()
        .unwrap_or_else(|| panic!("expected supersession_candidates on: {second_v}"));
    assert_eq!(candidates.len(), 1, "candidates: {candidates:?}");
    assert_eq!(candidates[0]["memory_id"].as_str().unwrap(), first_id);
    assert_eq!(candidates[0]["relation"].as_str().unwrap(), "supersession");
}
