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
/// `sh` before declaring the node failed and moving on — it kills only the
/// immediate `sh` child, not its process group. A backgrounded or
/// double-forked descendant (`cmd &`, `nohup`, etc.) can outlive the timeout
/// and keep running after the node is reported as timed out. Process-group
/// killing is deliberately out of scope for now; do not assume the timeout
/// bounds the spawned work itself, only this harness's wait on it.
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

        // Take the pipes and drain them on their own threads *before*
        // polling for exit. A child that writes more than the OS pipe
        // buffer (~64KB) blocks in write() until someone reads — if we only
        // read after the process exits, try_wait() never returns Some and a
        // large-but-legitimate command is misreported as a timeout.
        //
        // The reader threads report completion over an mpsc channel instead
        // of a JoinHandle, because `run:` can background a grandchild
        // (`cmd &`) that inherits these pipe write ends and outlives `sh`.
        // Killing (or even just waiting on) the immediate `sh` process does
        // not close that inherited fd, so a `read_to_end` never sees EOF and
        // a thread join would block forever. `recv_timeout` lets us bound
        // the wait and abandon the thread instead of hanging the whole run.
        //
        // Each thread accumulates into a shared buffer via incremental
        // `read()` calls rather than a single `read_to_end`, so that if the
        // grace period expires before EOF (an orphan is still holding the
        // pipe open), whatever bytes the *legitimate* process already wrote
        // are still visible in the buffer instead of being discarded along
        // with the abandoned thread.
        let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out_pipe = child.stdout.take().expect("piped");
        let (out_tx, out_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
        {
            let out_buf = std::sync::Arc::clone(&out_buf);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match out_pipe.read(&mut chunk) {
                        Ok(0) => {
                            let _ = out_tx.send(Ok(()));
                            break;
                        }
                        Ok(n) => out_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                        Err(e) => {
                            let _ = out_tx.send(Err(e));
                            break;
                        }
                    }
                }
            });
        }
        let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut err_pipe = child.stderr.take().expect("piped");
        let (err_tx, err_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
        {
            let err_buf = std::sync::Arc::clone(&err_buf);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match err_pipe.read(&mut chunk) {
                        Ok(0) => {
                            let _ = err_tx.send(Ok(()));
                            break;
                        }
                        Ok(n) => err_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                        Err(e) => {
                            let _ = err_tx.send(Err(e));
                            break;
                        }
                    }
                }
            });
        }

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
                        // The command is already declared failed, so this
                        // grace must not add meaningful latency — it only
                        // exists to pick up output that was already fully
                        // buffered before the kill landed. If an orphaned
                        // grandchild still holds the pipe open, the recv
                        // simply times out and that reader thread is
                        // abandoned (leaked, blocked forever, but bounded to
                        // one thread per affected run).
                        let timeout_grace = Duration::from_millis(50);
                        let _ = out_rx.recv_timeout(timeout_grace);
                        let _ = err_rx.recv_timeout(timeout_grace);
                        return Ok(NodeOutcome::Failed {
                            error: format!(
                                "command timed out after {:?}: {}",
                                self.timeout, req.run
                            ),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };

        // Normal-exit grace: the process has already exited, so under
        // ordinary circumstances the pipe is closed and the reader thread's
        // EOF signal has already landed (or is about to, essentially
        // immediately) — a couple of seconds is generous slack for a slow
        // scheduler, while still being bounded if an orphaned grandchild
        // (`cmd &`) is holding the write end open. In that orphan case we
        // don't fail the command solely because a stream couldn't be fully
        // drained; we take whatever the buffer holds so far and abandon
        // that reader thread.
        let normal_grace = Duration::from_secs(2);
        if let Ok(result) = out_rx.recv_timeout(normal_grace) {
            result.map_err(RunnerError::Io)?;
        }
        if let Ok(result) = err_rx.recv_timeout(normal_grace) {
            result.map_err(RunnerError::Io)?;
        }
        let out_snapshot = out_buf.lock().unwrap().clone();
        let err_snapshot = err_buf.lock().unwrap().clone();

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
