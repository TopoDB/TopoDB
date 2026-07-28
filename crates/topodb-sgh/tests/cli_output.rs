#![cfg(all(feature = "cli", feature = "claude-code"))]

//! Process-level pins on `sgh validate`'s output contract: failures must not
//! trail a success line. The rail/pairing checks run BEFORE "valid: N
//! node(s)" prints, so a caller (human or script) reading top-down never sees
//! success-then-error.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sgh"))
}

#[test]
fn validate_pairing_failure_prints_no_success_line_first() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("g.yaml");
    // tools: [topodb] with no --agent-mcp trips the pairing rule at the gate.
    std::fs::write(
        &graph,
        "version: 1\ngoal: pairing gate\nnodes:\n  - id: store\n    kind: agent\n    prompt: \"p\"\n    tools: [topodb]\n    budget: {retries: 0, repairs: 0}\n",
    )
    .unwrap();
    let out = bin().arg("validate").arg(&graph).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "pairing violation must fail validate"
    );
    assert!(
        !stdout.contains("valid:"),
        "success line must not precede the pairing failure; stdout: {stdout}"
    );
    assert!(
        stderr.contains("agent-mcp"),
        "failure must name the missing flag; stderr: {stderr}"
    );
}

#[test]
fn validate_clean_graph_still_prints_the_success_line() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("g.yaml");
    std::fs::write(
        &graph,
        "version: 1\ngoal: clean\nnodes:\n  - id: a\n    kind: command\n    run: \"true\"\n    budget: {retries: 0, repairs: 0}\n",
    )
    .unwrap();
    let out = bin().arg("validate").arg(&graph).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("valid: 1 node(s)"), "stdout: {stdout}");
}

#[test]
fn base_url_requires_openai_provider() {
    let out = bin()
        .args([
            "run",
            "graph.yaml",
            "--provider",
            "anthropic",
            "--base-url",
            "http://x",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--base-url applies only"));
}

#[test]
fn agent_bash_requires_claude_code_provider() {
    let out = bin()
        .args([
            "run",
            "graph.yaml",
            "--provider",
            "openai",
            "--agent-bash",
            "topodb",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--agent-bash applies only"));
}

#[test]
fn run_help_lists_the_hardening_flags() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sgh"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in ["--agent-timeout", "--max-inflight"] {
        assert!(help.contains(flag), "missing {flag} in run --help");
    }
}

#[test]
fn resume_help_exists_with_approve_gate() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sgh"))
        .args(["resume", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--approve-gate"));
    assert!(!help.contains("--replan"), "resume must not offer replan");
}

#[test]
fn resume_unknown_run_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sgh"))
        .args([
            "--db",
            dir.path().join("x.redb").to_str().unwrap(),
            "resume",
            "01NOPE",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}

#[test]
fn show_requires_run_id_or_list() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sgh"))
        .args(["show"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn show_missing_event_log_names_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sgh"))
        .args([
            "--db",
            dir.path().join("x.redb").to_str().unwrap(),
            "show",
            "01RUN",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("01RUN"));
}

#[test]
fn show_pretty_prints_a_canned_event_file() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("x.redb");
    let events_dir = dir.path().join("x.redb.events");
    std::fs::create_dir_all(&events_dir).unwrap();

    // Write a canned event file with three valid lines + one garbage line (forward-compat test)
    let event_file = events_dir.join("01RUN.jsonl");
    let events = r#"{"v":1,"ts":1000,"event":"run_started","run_id":"01RUN","goal":"test goal","agent_calls_bound":100,"command_runs_bound":10}
not json at all {
{"v":1,"ts":1100,"event":"node_started","node_id":"a"}
{"v":1,"ts":1200,"event":"node_succeeded","node_id":"a"}"#;
    std::fs::write(&event_file, events).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sgh"))
        .args(["--db", db_path.to_str().unwrap(), "show", "01RUN"])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Check exit code 0 and presence of all events
    assert!(stdout.contains("run_started"), "stdout: {stdout}");
    assert!(stdout.contains("node_started"), "stdout: {stdout}");
    assert!(stdout.contains("node_succeeded"), "stdout: {stdout}");

    // Forward-compat: unparseable line printed raw with ? prefix
    assert!(stdout.contains("? not json at all {"), "stdout: {stdout}");

    // Verify ordering via byte offsets
    let run_started_pos = stdout
        .find("run_started")
        .expect("run_started not found in output");
    let node_started_pos = stdout
        .find("node_started")
        .expect("node_started not found in output");
    let node_succeeded_pos = stdout
        .find("node_succeeded")
        .expect("node_succeeded not found in output");
    assert!(
        run_started_pos < node_started_pos && node_started_pos < node_succeeded_pos,
        "events not in order: run_started={}, node_started={}, node_succeeded={}",
        run_started_pos,
        node_started_pos,
        node_succeeded_pos
    );
}
