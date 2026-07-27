use std::process::Command;

use super::PlannerError;

pub use super::{BoundedPlanner, PlanBackend};

/// Type alias for the bounded planner over Claude backend.
pub type ClaudePlanner = BoundedPlanner;

/// Shells out to `claude -p`, mirroring `runner::claude::ClaudeCodeRunner`.
pub struct ClaudeBackend {
    pub model: Option<String>,
}

impl PlanBackend for ClaudeBackend {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p").arg(prompt);
        if let Some(m) = &self.model {
            cmd.arg("--model").arg(m);
        }
        let out = cmd
            .output()
            .map_err(|e| PlannerError::Runner(format!("spawning claude: {e}")))?;

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

/// The claude-cli planner: `BoundedPlanner` over `ClaudeBackend`.
pub fn claude_planner(model: Option<String>, max_attempts: u32) -> BoundedPlanner {
    BoundedPlanner::with_backend(Box::new(ClaudeBackend { model }), max_attempts)
}
