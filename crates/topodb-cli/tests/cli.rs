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

    // find (default spec has no equality index → Entity/name is undeclared → Rejected;
    // so this db was created with the DEFAULT spec. To exercise find, the db must be
    // created with a spec declaring (Entity,name). For v1 the CLI opens with open_stored,
    // and a default-spec db has no equality index — so `find` here is expected to Rejected.)
    let (_e, s) = full(&[
        "find", "--label", "Entity", "--prop", "name", "--value", "ada",
    ]);
    assert_eq!(
        s.code(),
        Some(2),
        "default-spec db: Entity/name not equality-indexed"
    );

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
