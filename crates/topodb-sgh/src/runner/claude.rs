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

/// Decide what a completed `claude -p` invocation actually accomplished.
///
/// Exit status alone cannot answer this. When a tool call is blocked, `claude`
/// reports `subtype: "success"`, `is_error: false`, and exits 0 — the denial
/// appears only in `permission_denials`. A node whose Write was denied changed
/// nothing, so trusting the exit code records a no-op as completed work, and a
/// run that produced no output becomes indistinguishable from one that did the
/// whole job.
pub fn interpret_result(stdout: &str) -> NodeOutcome {
    let v: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            // Unreadable output is not evidence of work. Failing here is the
            // conservative reading: it surfaces a broken invocation instead of
            // passing an unexamined string downstream as though it were a
            // result.
            return NodeOutcome::Failed {
                error: format!("claude produced unparseable output ({e}): {}", elide(stdout)),
            };
        }
    };

    let denied: Vec<&str> = v
        .get("permission_denials")
        .and_then(|d| d.as_array())
        .map(|d| {
            d.iter()
                .filter_map(|x| x.get("tool_name").and_then(|t| t.as_str()))
                .collect()
        })
        .unwrap_or_default();

    if !denied.is_empty() {
        return NodeOutcome::Failed {
            error: format!(
                "claude was denied {} — the node cannot have done its work. \
                 Grant the tool in ClaudeCodeRunner, or move the work to a \
                 `command` node whose `run:` string passes through the approval gate.",
                denied.join(", ")
            ),
        };
    }

    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        let detail = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
        return NodeOutcome::Failed {
            error: format!("claude reported an error: {}", detail.trim()),
        };
    }

    match v.get("result").and_then(|r| r.as_str()) {
        Some(r) => NodeOutcome::Succeeded {
            output: r.trim().to_string(),
        },
        None => NodeOutcome::Failed {
            error: format!("claude returned no `result` field: {}", elide(stdout)),
        },
    }
}

/// Keep a diagnostic short enough to read in a run report.
fn elide(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    let head: String = s.chars().take(200).collect();
    format!("{head}…")
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
        // smallest grant that lets a node read and edit source.
        //
        // It does NOT confine the node to these three tools. `--allowedTools`
        // is additive — it grants on top of the user's settings and restricts
        // nothing. Verified: with `--allowedTools Read`, an agent asked to run
        // `echo probe` via Bash still ran it, with no entry in
        // `permission_denials`. So an agent node can reach whatever the
        // ambient settings already permit, Bash included, and omitting a tool
        // here withholds nothing. Confining a node to a tool set would need a
        // mechanism this flag does not provide.
        cmd.arg("--allowedTools").arg("Read,Write,Edit");
        // Structured output is what makes a denied tool visible at all: in
        // plain-text mode a blocked Write is indistinguishable from a
        // completed one, since both exit 0 with prose on stdout.
        cmd.arg("--output-format").arg("json");
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
        Ok(interpret_result(&stdout))
    }
}
