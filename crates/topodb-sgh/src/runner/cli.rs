//! One AgentRunner for every print-mode CLI backend. Owns spawn (own process
//! group), deadline wait, group kill, and capture via runner::proc; a CliCodec
//! only builds argv and interprets the captured output.
use std::process::{Command, ExitStatus};
use std::time::Duration;

use super::cancel::CancelToken;
use super::proc::{self, ProcEnd};
use super::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};

const DEFAULT_DEADLINE: Duration = Duration::from_secs(600);

pub trait CliCodec: Send + Sync {
    fn argv(&self, req: &NodeRequest) -> Vec<String>;
    /// Interpret a completed invocation. Raw bytes: the codec owns UTF-8
    /// policy (claude's: non-zero exit reports stderr lossily; zero exit
    /// with non-UTF-8 stdout is RunnerError::Utf8 — moved here verbatim).
    fn interpret(
        &self,
        req: &NodeRequest,
        exit: ExitStatus,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<NodeOutcome, RunnerError>;
}

pub struct CliPrintRunner {
    codec: Box<dyn CliCodec>,
    pub deadline: Duration,
    pub cancel: Option<CancelToken>,
}

impl CliPrintRunner {
    pub fn new(codec: Box<dyn CliCodec>) -> Self {
        CliPrintRunner {
            codec,
            deadline: DEFAULT_DEADLINE,
            cancel: None,
        }
    }

    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }

    pub fn with_cancel(mut self, t: CancelToken) -> Self {
        self.cancel = Some(t);
        self
    }
}

impl AgentRunner for CliPrintRunner {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        let argv = self.codec.argv(req);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let (out, end) = proc::run_with_deadline(&mut cmd, self.deadline, self.cancel.as_ref())?;

        match end {
            ProcEnd::DeadlineKilled => Ok(NodeOutcome::Failed {
                error: format!("deadline exceeded after {}s", self.deadline.as_secs()),
            }),
            ProcEnd::Cancelled => Ok(NodeOutcome::Failed {
                error: "cancelled".into(),
            }),
            ProcEnd::Exited => self
                .codec
                .interpret(req, out.status, &out.stdout, &out.stderr),
        }
    }
}
