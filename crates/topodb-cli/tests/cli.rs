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
