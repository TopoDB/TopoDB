use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use topodb_sgh::runner::command::{
    env_var_name, CommandRequest, CommandRunner, ShellCommandRunner,
};
use topodb_sgh::runner::NodeOutcome;

fn req(run: &str) -> CommandRequest {
    CommandRequest {
        node_id: "c".into(),
        run: run.into(),
        inputs: BTreeMap::new(),
        output_schema: None,
    }
}

fn runner() -> ShellCommandRunner {
    ShellCommandRunner::new(Duration::from_secs(10))
}

#[test]
fn sanitizes_dependency_ids_into_env_var_names() {
    assert_eq!(env_var_name("survey"), "SGH_INPUT_SURVEY");
    assert_eq!(env_var_name("find-call-sites"), "SGH_INPUT_FIND_CALL_SITES");
    assert_eq!(env_var_name("step.2"), "SGH_INPUT_STEP_2");
}

#[test]
fn successful_command_without_schema_reports_stdout_and_exit_code() {
    match runner().run(&req("echo hello")).unwrap() {
        NodeOutcome::Succeeded { output } => {
            let v: serde_json::Value = serde_json::from_str(&output).expect("valid json");
            assert_eq!(v["stdout"], "hello");
            assert_eq!(v["exit_code"], 0);
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn nonzero_exit_is_a_failed_outcome_carrying_stderr() {
    match runner().run(&req("echo boom >&2; exit 3")).unwrap() {
        NodeOutcome::Failed { error } => {
            assert!(error.contains("boom"), "stderr must survive: {error}");
            assert!(error.contains('3'), "exit code must be reported: {error}");
        }
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn a_failing_command_with_non_utf8_stdout_reports_exit_status_not_a_utf8_error() {
    // Regression: stdout must never be decoded before the exit status is
    // checked. A failing command's non-UTF-8 stdout must not surface as
    // RunnerError::Utf8, shadowing the informative "exited with N: <stderr>"
    // that the recovery ladder and replan context depend on.
    let r = req("printf 'boom' >&2; printf '\\xff\\xfe' ; exit 7");
    match runner().run(&r).unwrap() {
        NodeOutcome::Failed { error } => {
            assert!(
                error.contains("exited with 7"),
                "exit code must be reported: {error}"
            );
            assert!(error.contains("boom"), "stderr must survive: {error}");
        }
        other => panic!("expected a Failed outcome, not an Err, got {other:?}"),
    }
}

#[test]
fn declared_inputs_are_exported_as_environment_variables() {
    let mut inputs = BTreeMap::new();
    inputs.insert("survey".to_string(), r#"{"sites":2}"#.to_string());
    let r = CommandRequest {
        node_id: "c".into(),
        run: "printf '%s' \"$SGH_INPUT_SURVEY\"".into(),
        inputs,
        output_schema: None,
    };

    match runner().run(&r).unwrap() {
        NodeOutcome::Succeeded { output } => {
            let v: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(v["stdout"], r#"{"sites":2}"#);
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn a_command_with_a_declared_schema_returns_stdout_verbatim() {
    let r = CommandRequest {
        node_id: "c".into(),
        run: "echo '{\"sites\":2}'".into(),
        inputs: BTreeMap::new(),
        output_schema: Some(serde_json::json!({"type": "object"})),
    };

    match runner().run(&r).unwrap() {
        NodeOutcome::Succeeded { output } => {
            let v: serde_json::Value = serde_json::from_str(&output).expect("valid json");
            assert_eq!(
                v,
                serde_json::json!({"sites": 2}),
                "output must not be wrapped"
            );
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn a_command_producing_well_over_the_pipe_buffer_succeeds_rather_than_timing_out() {
    // ~200KB of stdout, well beyond the ~64KB OS pipe buffer. A poll loop
    // that doesn't drain the pipe concurrently with waiting deadlocks here
    // and reports a timeout even though the command would have succeeded.
    let r = CommandRequest {
        node_id: "c".into(),
        run: "head -c 200000 /dev/zero | tr '\\0' 'x'".into(),
        inputs: BTreeMap::new(),
        output_schema: None,
    };

    // Generous timeout: this test is about the deadlock, not about slowness.
    let big_runner = ShellCommandRunner::new(Duration::from_secs(30));
    match big_runner.run(&r).unwrap() {
        NodeOutcome::Succeeded { output } => {
            let v: serde_json::Value = serde_json::from_str(&output).expect("valid json");
            let stdout = v["stdout"].as_str().expect("stdout is a string");
            assert_eq!(stdout.len(), 200_000);
            assert!(stdout.chars().all(|c| c == 'x'));
        }
        other => panic!("expected success, got {other:?}"),
    }
}

#[test]
fn a_command_exceeding_the_timeout_fails_rather_than_hanging() {
    let r = ShellCommandRunner::new(Duration::from_millis(200));
    match r.run(&req("sleep 5")).unwrap() {
        NodeOutcome::Failed { error } => assert!(
            error.to_lowercase().contains("timed out"),
            "timeout must be named in the error: {error}"
        ),
        other => panic!("expected timeout failure, got {other:?}"),
    }
}

#[test]
fn a_backgrounded_grandchild_does_not_block_the_success_path() {
    // `sleep 30 &` backgrounds a grandchild that inherits the stdout/stderr
    // pipe write ends and outlives `sh`. `sh -c` itself exits immediately
    // after starting it, so the command succeeds right away — but a reader
    // thread joined unconditionally would still block until that orphaned
    // `sleep` exits (or forever, for a true daemon).
    // See the analogous proc_engine test for why `$!` (not `$$`) is used to
    // capture the real grandchild pid on macOS's bash-based `/bin/sh`.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = format!(
        "(exec sleep 30) & echo $! > {}; echo done",
        pidfile.display()
    );
    let r = ShellCommandRunner::new(Duration::from_secs(10));
    let started = Instant::now();
    match r.run(&req(&script)).unwrap() {
        NodeOutcome::Succeeded { output } => {
            let v: serde_json::Value = serde_json::from_str(&output).expect("valid json");
            assert_eq!(v["stdout"], "done");
        }
        other => panic!("expected success, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait on the orphaned grandchild's pipe: took {:?}",
        started.elapsed()
    );
    #[cfg(unix)]
    {
        // The group kill only fires on the timeout path; on the success
        // path `sh` exits on its own without ever killing anything, so the
        // backgrounded grandchild is expected to still be alive here. This
        // just confirms the pidfile/probe technique itself is sound and the
        // grandchild really was running (i.e. the success-path assertions
        // above are exercising a live orphan, not a no-op).
        std::thread::sleep(Duration::from_millis(200));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            alive,
            "grandchild {pid} should still be running (not group-killed on the success path)"
        );
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[test]
fn a_backgrounded_grandchild_does_not_block_the_timeout_path() {
    // Same orphan hazard, but this time `sh` itself is also still running
    // (a second foreground `sleep`) when the runner's short timeout fires
    // and kills it. The backgrounded `sleep 30` still holds the pipe open
    // after the kill, so the post-kill drain must not block on it either.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let script = format!(
        "(exec sleep 30) & echo $! > {}; sleep 30",
        pidfile.display()
    );
    let r = ShellCommandRunner::new(Duration::from_millis(300));
    let started = Instant::now();
    match r.run(&req(&script)).unwrap() {
        NodeOutcome::Failed { error } => assert!(
            error.to_lowercase().contains("timed out"),
            "timeout must be named in the error: {error}"
        ),
        other => panic!("expected timeout failure, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait on the orphaned grandchild's pipe: took {:?}",
        started.elapsed()
    );
    #[cfg(unix)]
    {
        // On the timeout path the whole process group is killed, so the
        // backgrounded grandchild must be dead too, not merely the
        // immediate `sh` child.
        std::thread::sleep(Duration::from_millis(200));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "grandchild {pid} survived the timeout's group kill");
    }
}
