use std::process::Command;

use super::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};

/// Assembles the prompt for a node. Kept separate from process spawning so it
/// is unit-testable without invoking a model.
pub fn build_prompt(req: &NodeRequest) -> String {
    let mut p = String::new();
    p.push_str(&req.prompt);

    if !req.inputs.is_empty() {
        p.push_str("\n\n## Inputs\n\n");
        p.push_str(
            "These are the complete outputs of this step's declared dependencies. \
             They are the only context from the run available to you.\n\n",
        );
        for (id, json) in &req.inputs {
            p.push_str(&format!("### {id}\n\n```json\n{json}\n```\n\n"));
        }
    }

    if let Some(schema) = &req.output_schema {
        p.push_str("\n\n## Required output\n\n");
        p.push_str(
            "Reply with bare JSON matching this schema and nothing else — no prose, \
             no code fences. Output that does not match is treated as a failure.\n\n",
        );
        p.push_str(&serde_json::to_string_pretty(schema).unwrap_or_default());
        p.push('\n');
    }

    p
}

pub struct ClaudeCodeRunner {
    model: Option<String>,
}

impl ClaudeCodeRunner {
    pub fn new(model: Option<String>) -> Self {
        ClaudeCodeRunner { model }
    }
}

impl AgentRunner for ClaudeCodeRunner {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p").arg(build_prompt(req));
        // Without a tool grant, an agent node runs under the default
        // permission mode, where there is no one to approve a Write. The tool
        // call is blocked, the agent explains that it was blocked, and
        // `claude` still exits 0 — so the node is recorded as succeeded having
        // changed nothing. An agent node whose purpose is to edit files needs
        // the grant up front or it cannot do its job.
        //
        // Enumerated rather than `--permission-mode acceptEdits`: this is the
        // smallest grant that lets a node read and edit source, and it
        // withholds Bash, so an agent node still cannot run arbitrary
        // commands. Shell execution stays with `command` nodes, whose `run:`
        // strings pass through the /sgh:run approval gate.
        cmd.arg("--allowedTools").arg("Read,Write,Edit");
        if let Some(m) = &self.model {
            cmd.arg("--model").arg(m);
        }

        let out = cmd.output()?;

        // Check the exit status before decoding stdout. A failing
        // invocation's stdout is not a promise of valid UTF-8 (partial
        // writes, binary diagnostics, etc.), and decoding it first would
        // turn a diagnosable failure (exit status + stderr) into a
        // confusing `RunnerError::Utf8` that discards both.
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            return Ok(NodeOutcome::Failed {
                error: format!("claude exited with {}: {}", out.status, stderr.trim()),
            });
        }

        let stdout = String::from_utf8(out.stdout).map_err(|_| RunnerError::Utf8)?;
        Ok(NodeOutcome::Succeeded {
            output: stdout.trim().to_string(),
        })
    }
}
