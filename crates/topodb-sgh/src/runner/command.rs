use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{NodeOutcome, RunnerError};

/// Everything a command node is allowed to see. Like `NodeRequest`, `inputs`
/// carries exactly the declared upstream outputs and nothing else.
#[derive(Debug, Clone)]
pub struct CommandRequest {
    pub node_id: String,
    pub run: String,
    /// upstream node id -> that node's output JSON
    pub inputs: BTreeMap<String, String>,
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

        let mut child = cmd.spawn()?;

        // Poll for completion so a hung command cannot stall the run. A
        // timeout is a Failed outcome, not an Err, so the recovery ladder
        // sees it and the run stays bounded.
        let started = Instant::now();
        let status = loop {
            match child.try_wait()? {
                Some(status) => break status,
                None => {
                    if started.elapsed() >= self.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(NodeOutcome::Failed {
                            error: format!(
                                "command timed out after {}s: {}",
                                self.timeout.as_secs(),
                                req.run
                            ),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut out) = child.stdout.take() {
            let mut buf = Vec::new();
            out.read_to_end(&mut buf)?;
            stdout = String::from_utf8(buf).map_err(|_| RunnerError::Utf8)?;
        }
        if let Some(mut err) = child.stderr.take() {
            let mut buf = Vec::new();
            err.read_to_end(&mut buf)?;
            stderr = String::from_utf8_lossy(&buf).into_owned();
        }

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Ok(NodeOutcome::Failed {
                error: format!("command exited with {code}: {}", stderr.trim()),
            });
        }

        // No declared schema: emit a structured, valid-JSON default. With a
        // declared schema the executor validates stdout directly, so the
        // command's own JSON must pass through untouched.
        let trimmed = stdout.trim();
        let output = serde_json::json!({
            "stdout": trimmed,
            "exit_code": status.code().unwrap_or(0),
        })
        .to_string();

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
