use std::collections::BTreeMap;
use std::time::Duration;

use topodb_sgh::runner::command::{env_var_name, CommandRequest, CommandRunner, ShellCommandRunner};
use topodb_sgh::runner::NodeOutcome;

fn req(run: &str) -> CommandRequest {
    CommandRequest { node_id: "c".into(), run: run.into(), inputs: BTreeMap::new() }
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
fn declared_inputs_are_exported_as_environment_variables() {
    let mut inputs = BTreeMap::new();
    inputs.insert("survey".to_string(), r#"{"sites":2}"#.to_string());
    let r = CommandRequest {
        node_id: "c".into(),
        run: "printf '%s' \"$SGH_INPUT_SURVEY\"".into(),
        inputs,
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
