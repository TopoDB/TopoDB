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
