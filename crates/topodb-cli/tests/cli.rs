use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_topodb"))
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

#[test]
fn missing_db_flag_is_usage_error_exit_2() {
    let out = bin().arg("info").output().unwrap();
    assert_eq!(out.status.code(), Some(2)); // clap usage error
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
    let id = full(&["create-entity", "--name", "ada", "--props", r#"{"stale":"yes"}"#]).0["id"]
        .as_str()
        .unwrap()
        .to_string();
    // set one key, remove another (null).
    let (res, s) = full(&["set-props", &id, "--props", r#"{"role":"pioneer","stale":null}"#]);
    assert!(s.success(), "set-props should succeed");
    assert!(res["seq"].as_u64().is_some());
    let node = full(&["get", &id]).0;
    assert_eq!(node["node"]["props"]["role"], serde_json::json!("pioneer"));
    assert!(node["node"]["props"].get("stale").is_none(), "stale should be removed");
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
    let id = full(&["create-entity", "--name", "gone"]).0["id"].as_str().unwrap().to_string();
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
    let a = full(&["create-entity", "--name", "a"]).0["id"].as_str().unwrap().to_string();
    let b = full(&["create-entity", "--name", "b"]).0["id"].as_str().unwrap().to_string();
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
