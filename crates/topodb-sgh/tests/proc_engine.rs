//! The shared process engine: deadline, group kill, drain, cancellation.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use topodb_sgh::runner::cancel::CancelToken;
use topodb_sgh::runner::proc::{run_with_deadline, ProcEnd};

fn sh(script: &str) -> Command {
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

#[test]
fn normal_exit_captures_output() {
    let (out, end) = run_with_deadline(
        &mut sh("echo hi; echo err >&2"),
        Duration::from_secs(10),
        None,
    )
    .unwrap();
    assert_eq!(end, ProcEnd::Exited);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
}

#[test]
fn deadline_kills_and_returns_quickly() {
    let started = Instant::now();
    let (_, end) =
        run_with_deadline(&mut sh("sleep 30"), Duration::from_millis(300), None).unwrap();
    assert_eq!(end, ProcEnd::DeadlineKilled);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn deadline_kills_the_whole_group_including_grandchildren() {
    // The grandchild writes a pidfile; after the group kill, that pid must be gone.
    // `$$` inside a `(...)` subshell is unreliable here: on macOS's default
    // `/bin/sh` (bash in posix mode), `$$` inside a subshell still reports
    // the *parent* shell's pid, not the subshell's own — so a pidfile
    // written that way records the wrong process and the probe below would
    // trivially "pass" by observing the already-reaped parent instead of
    // the actual grandchild. `$!` after backgrounding gives the real forked
    // pid of the subshell, which `exec` then turns into `sleep` without
    // changing its pid.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = format!(
        "(exec sleep 30) & echo $! > {}; sleep 30",
        pidfile.display()
    );
    let (_, end) = run_with_deadline(&mut sh(&script), Duration::from_millis(500), None).unwrap();
    assert_eq!(end, ProcEnd::DeadlineKilled);
    // Give the OS a moment, then probe the grandchild with signal 0.
    std::thread::sleep(Duration::from_millis(200));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "grandchild {pid} survived the group kill");
}

#[test]
fn cancellation_kills_and_reports_cancelled() {
    let token = CancelToken::new();
    let t2 = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        t2.cancel();
    });
    let started = Instant::now();
    let (_, end) =
        run_with_deadline(&mut sh("sleep 30"), Duration::from_secs(30), Some(&token)).unwrap();
    assert_eq!(end, ProcEnd::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn large_output_drains_without_hanging() {
    let (out, end) = run_with_deadline(
        &mut sh("dd if=/dev/zero bs=1024 count=200 2>/dev/null | tr '\\0' 'x'"),
        Duration::from_secs(30),
        None,
    )
    .unwrap();
    assert_eq!(end, ProcEnd::Exited);
    assert_eq!(out.stdout.len(), 200 * 1024);
}
