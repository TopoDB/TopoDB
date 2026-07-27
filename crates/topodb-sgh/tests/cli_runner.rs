//! CliPrintRunner against a fake codec + real subprocesses.
use std::collections::BTreeMap;
use std::process::ExitStatus;
use std::time::{Duration, Instant};
use topodb_sgh::runner::cli::{CliCodec, CliPrintRunner};
use topodb_sgh::runner::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};

struct EchoCodec {
    script: String,
}
impl CliCodec for EchoCodec {
    fn argv(&self, _req: &NodeRequest) -> Vec<String> {
        vec!["sh".into(), "-c".into(), self.script.clone()]
    }
    fn interpret(
        &self,
        _req: &NodeRequest,
        exit: ExitStatus,
        stdout: &[u8],
        _stderr: &[u8],
    ) -> Result<NodeOutcome, RunnerError> {
        if exit.success() {
            Ok(NodeOutcome::Succeeded {
                output: String::from_utf8_lossy(stdout).trim().to_string(),
            })
        } else {
            Ok(NodeOutcome::Failed {
                error: format!("exit {exit}"),
            })
        }
    }
}

fn req() -> NodeRequest {
    NodeRequest {
        node_id: "n".into(),
        prompt: "p".into(),
        inputs: BTreeMap::new(),
        output_schema: None,
        tools: vec![],
    }
}

#[test]
fn success_flows_through_codec() {
    let r = CliPrintRunner::new(Box::new(EchoCodec {
        script: "echo out".into(),
    }));
    assert_eq!(
        r.run(&req()).unwrap(),
        NodeOutcome::Succeeded {
            output: "out".into()
        }
    );
}

#[test]
fn nonzero_exit_flows_through_codec() {
    let r = CliPrintRunner::new(Box::new(EchoCodec {
        script: "exit 3".into(),
    }));
    match r.run(&req()).unwrap() {
        NodeOutcome::Failed { error } => assert!(error.contains("exit"), "got: {error}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn deadline_produces_the_specified_failure_string() {
    let r = CliPrintRunner::new(Box::new(EchoCodec {
        script: "sleep 30".into(),
    }))
    .with_deadline(Duration::from_millis(300));
    let started = Instant::now();
    match r.run(&req()).unwrap() {
        NodeOutcome::Failed { error } => assert_eq!(error, "deadline exceeded after 0s"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(5));
}
