//! Tests for binary process arguments like --help, --version.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
}

/// The `--help` flag should print usage to stdout and exit 0.
#[test]
fn help_flag_prints_usage_and_exits_0() {
    let out = bin().arg("--help").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "--help should exit 0, got exit code {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("usage"),
        "stdout should contain 'usage', got: {stdout}"
    );
}

/// The `-h` short form should also print usage to stdout and exit 0.
#[test]
fn h_short_flag_prints_usage_and_exits_0() {
    let out = bin().arg("-h").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "-h should exit 0, got exit code {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("usage"),
        "stdout should contain 'usage', got: {stdout}"
    );
}

/// The `--version` flag should print version to stdout and exit 0.
#[test]
fn version_flag_prints_version_and_exits_0() {
    let out = bin().arg("--version").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "--version should exit 0, got exit code {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("topodb-mcp "),
        "stdout should start with 'topodb-mcp ', got: {stdout}"
    );
}

/// The `-V` short form should also print version to stdout and exit 0.
#[test]
fn v_short_flag_prints_version_and_exits_0() {
    let out = bin().arg("-V").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "-V should exit 0, got exit code {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("topodb-mcp "),
        "stdout should start with 'topodb-mcp ', got: {stdout}"
    );
}
