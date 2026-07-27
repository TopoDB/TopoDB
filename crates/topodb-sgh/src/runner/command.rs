use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::proc::{run_with_deadline, ProcEnd};
use super::{NodeOutcome, RunnerError};

/// Everything a command node is allowed to see. Like `NodeRequest`, `inputs`
/// carries exactly the declared upstream outputs and nothing else.
#[derive(Debug, Clone)]
pub struct CommandRequest {
    pub node_id: String,
    pub run: String,
    /// upstream node id -> that node's output JSON
    pub inputs: BTreeMap<String, String>,
    /// Mirrors `NodeRequest::output_schema`. This is purely a wrap/don't-wrap
    /// signal for the runner: `Some(_)` means stdout is returned verbatim so
    /// the executor's `validate_output` can check it against the schema
    /// exactly like an agent node's output. `None` means stdout is wrapped in
    /// `{"stdout":.., "exit_code":..}`. The runner never validates the schema
    /// itself — that responsibility stays in the executor.
    pub output_schema: Option<serde_json::Value>,
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, req: &CommandRequest) -> Result<NodeOutcome, RunnerError>;
}

/// Dependency ids are free-form but environment variable names are not.
/// Uppercase, and replace anything outside `A-Z0-9` with `_`.
pub fn env_var_name(dep_id: &str) -> String {
    let mut s = String::from("SGH_INPUT_");
    for ch in dep_id.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_uppercase());
        } else {
            s.push('_');
        }
    }
    s
}

/// Runs a command node's `run:` string through `sh -c`.
///
/// Shell semantics are deliberate — pipes and redirection are most of what
/// makes command nodes useful. The control on model-authored commands is the
/// approval gate, which displays every `run:` string before any execution and
/// which a replan revision must re-enter.
///
/// `--command-timeout` bounds how long *this harness* waits on the spawned
/// `sh` before declaring the node failed and moving on. On unix, `sh` is
/// spawned as the leader of its own process group, and a timeout kills that
/// whole group (`killpg`) — so a backgrounded or double-forked descendant
/// (`cmd &`, `nohup`, etc.) is killed along with `sh` rather than outliving
/// the timeout. On non-unix targets there is no process-group kill available
/// through the standard library, so only the immediate `sh` child is
/// killed; a backgrounded descendant there can still outlive the timeout.
pub struct ShellCommandRunner {
    timeout: Duration,
}

impl ShellCommandRunner {
    pub fn new(timeout: Duration) -> Self {
        ShellCommandRunner { timeout }
    }
}

impl CommandRunner for ShellCommandRunner {
    fn run(&self, req: &CommandRequest) -> Result<NodeOutcome, RunnerError> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&req.run)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        for (dep, json) in &req.inputs {
            cmd.env(env_var_name(dep), json);
        }

        // Spawn, drain, poll, and (on timeout) group-kill are all handled by
        // the shared engine — see `runner::proc` for the drain-thread and
        // grace-period rationale.
        let (out, end) = run_with_deadline(&mut cmd, self.timeout, None)?;

        if end == ProcEnd::DeadlineKilled {
            return Ok(NodeOutcome::Failed {
                error: format!("command timed out after {:?}: {}", self.timeout, req.run),
            });
        }

        let status = out.status;
        let out_snapshot = out.stdout;
        let err_snapshot = out.stderr;

        // Exit status is checked before stdout is ever decoded as UTF-8. A
        // failing command's stdout is never inspected, so non-UTF-8 stdout
        // from a *failing* command must not shadow the informative "exited
        // with N: <stderr>" behind a Utf8 error — stderr is always
        // lossy-decoded, so it can never itself produce that error.
        let stderr = String::from_utf8_lossy(&err_snapshot).into_owned();

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Ok(NodeOutcome::Failed {
                error: format!("command exited with {code}: {}", stderr.trim()),
            });
        }

        let stdout = String::from_utf8(out_snapshot).map_err(|_| RunnerError::Utf8)?;
        let trimmed = stdout.trim();
        let output = if req.output_schema.is_some() {
            // Declared schema: the executor validates stdout directly against
            // it via `validate_output`, so the command's own JSON must pass
            // through untouched. The runner does not itself check the schema.
            trimmed.to_string()
        } else {
            // No declared schema: emit a structured, valid-JSON default.
            serde_json::json!({
                "stdout": trimmed,
                "exit_code": status.code().unwrap_or(0),
            })
            .to_string()
        };

        Ok(NodeOutcome::Succeeded { output })
    }
}

/// Scripted command outcomes with no process spawn — the command-side
/// counterpart to `MockRunner`, so executor tests stay hermetic.
#[derive(Default)]
pub struct MockCommandRunner {
    scripts: std::sync::Mutex<std::collections::HashMap<String, Vec<NodeOutcome>>>,
    cursors: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl MockCommandRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue outcomes for a node. Once exhausted the last outcome repeats
    /// forever, so a permanently-failing command is one entry.
    pub fn script(self, node_id: &str, outcomes: Vec<NodeOutcome>) -> Self {
        self.scripts
            .lock()
            .unwrap()
            .insert(node_id.to_string(), outcomes);
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl CommandRunner for MockCommandRunner {
    fn run(&self, req: &CommandRequest) -> Result<NodeOutcome, RunnerError> {
        self.calls.lock().unwrap().push(req.node_id.clone());

        let scripts = self.scripts.lock().unwrap();
        let Some(outcomes) = scripts.get(&req.node_id) else {
            return Ok(NodeOutcome::Succeeded {
                output: r#"{"stdout":"","exit_code":0}"#.to_string(),
            });
        };
        if outcomes.is_empty() {
            return Ok(NodeOutcome::Succeeded {
                output: r#"{"stdout":"","exit_code":0}"#.to_string(),
            });
        }

        let mut cursors = self.cursors.lock().unwrap();
        let cursor = cursors.entry(req.node_id.clone()).or_insert(0);
        let idx = (*cursor).min(outcomes.len() - 1);
        *cursor += 1;
        Ok(outcomes[idx].clone())
    }
}
