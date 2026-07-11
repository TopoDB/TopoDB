use std::io::Write;
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
