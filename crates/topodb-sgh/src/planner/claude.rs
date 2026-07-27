use std::process::{Command, Stdio};
use std::time::Duration;

use crate::runner::cancel::CancelToken;
use crate::runner::proc::{self, ProcEnd};

use super::PlannerError;

pub use super::{BoundedPlanner, PlanBackend};

/// Type alias for the bounded planner over Claude backend.
pub type ClaudePlanner = BoundedPlanner;

/// Default whole-invocation deadline for `ClaudeBackend::complete`, mirroring
/// `HttpChatRunner::node_deadline`'s default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Shells out to `claude -p`, mirroring `runner::claude::ClaudeCodeRunner`.
pub struct ClaudeBackend {
    pub model: Option<String>,
    /// Whole-invocation deadline; the child (and its whole process group, on
    /// unix) is killed if `claude -p` has not exited by this point. Default
    /// 600s (see `Default` impl).
    pub timeout: Duration,
    /// Ctrl-C wiring: when set, the child (and its process group, on unix)
    /// is killed as soon as the token is cancelled, instead of only at the
    /// timeout. `None` (the default) preserves prior behavior — no
    /// cancellation surface, deadline-only.
    pub cancel: Option<CancelToken>,
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        ClaudeBackend {
            model: None,
            timeout: DEFAULT_TIMEOUT,
            cancel: None,
        }
    }
}

impl PlanBackend for ClaudeBackend {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p").arg(prompt);
        if let Some(m) = &self.model {
            cmd.arg("--model").arg(m);
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let (out, end) = proc::run_with_deadline(&mut cmd, self.timeout, self.cancel.as_ref())
            .map_err(|e| PlannerError::Runner(format!("spawning claude: {e}")))?;

        match end {
            ProcEnd::DeadlineKilled => {
                return Err(PlannerError::Runner(format!(
                    "deadline exceeded after {}s",
                    self.timeout.as_secs()
                )));
            }
            ProcEnd::Cancelled => {
                return Err(PlannerError::Runner("cancelled".into()));
            }
            ProcEnd::Exited => {}
        }

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            return Err(PlannerError::Runner(format!(
                "claude exited with {}: {}",
                out.status,
                stderr.trim()
            )));
        }
        String::from_utf8(out.stdout)
            .map_err(|_| PlannerError::Runner("claude produced invalid utf-8".into()))
    }
}

/// The claude-cli planner: `BoundedPlanner` over `ClaudeBackend`, default
/// timeout.
pub fn claude_planner(model: Option<String>, max_attempts: u32) -> BoundedPlanner {
    claude_planner_with_timeout(model, max_attempts, DEFAULT_TIMEOUT)
}

/// Same as `claude_planner`, with an explicit whole-invocation deadline
/// (wired from `--agent-timeout`).
pub fn claude_planner_with_timeout(
    model: Option<String>,
    max_attempts: u32,
    timeout: Duration,
) -> BoundedPlanner {
    BoundedPlanner::with_backend(
        Box::new(ClaudeBackend {
            model,
            timeout,
            cancel: None,
        }),
        max_attempts,
    )
}

/// Same as `claude_planner_with_timeout`, additionally wiring a `CancelToken`
/// so a Ctrl-C mid-replan kills the `claude -p` subprocess instead of
/// leaving it orphaned and the caller blocked on its exit.
pub fn claude_planner_with_timeout_and_cancel(
    model: Option<String>,
    max_attempts: u32,
    timeout: Duration,
    cancel: CancelToken,
) -> BoundedPlanner {
    BoundedPlanner::with_backend(
        Box::new(ClaudeBackend {
            model,
            timeout,
            cancel: Some(cancel),
        }),
        max_attempts,
    )
}
