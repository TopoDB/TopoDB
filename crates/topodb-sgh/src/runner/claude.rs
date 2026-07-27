use std::process::Command;

use super::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};

pub use super::common::{build_prompt, extract_json};
pub use super::rails::{validate_bash_grant, validate_mcp_server_command};
use super::common::elide;

/// Run-level MCP wiring for opted-in agent nodes: the path of the generated
/// mcp-config file naming the `topodb` stdio server. Presence of a value
/// means the RUN supplies the capability; whether a given node sees it is
/// the node's own `tools` opt-in (checked by the caller of [`build_argv`]).
pub struct McpWiring {
    pub config_path: String,
}

/// Build the command-line arguments for invoking `claude -p`.
///
/// Returns a vector of arguments suitable for `std::process::Command`.
/// Includes the prompt, allowedTools (with optional bash grants and MCP tools),
/// output format, and model if specified.
///
/// Structured output (--output-format json) is what makes a denied tool visible at all:
/// in plain-text mode a blocked tool call is indistinguishable from a completed one,
/// since both exit 0 with prose on stdout. This ensures that when a node's Write is
/// denied, we can detect it in the JSON response's permission_denials field.
///
/// The `mcp` parameter adds `mcp__topodb` to allowedTools and `--mcp-config <path>` to argv;
/// `None` keeps the argv byte-identical to the legacy behavior.
pub fn build_argv(
    prompt: String,
    model: Option<String>,
    bash_grants: &[String],
    mcp: Option<&McpWiring>,
) -> Vec<String> {
    let mut argv = vec!["claude".to_string(), "-p".to_string(), prompt];

    // Claude Code permission-rule syntax: Bash(<prefix>:*) is the documented
    // prefix-matching rule form used in settings allowlists (the same grammar
    // as settings.json "permissions.allow" entries). Verified against Claude
    // Code's permission-rules documentation; the repo itself has no prior
    // allowedTools usage to mirror.
    let mut allowed_tools = "Read,Write,Edit".to_string();
    for grant in bash_grants {
        allowed_tools.push_str(&format!(",Bash({}:*)", grant));
    }
    // `mcp__topodb` grants the whole topodb server's tools (decision: full
    // surface). Additive like everything else in --allowedTools; the server
    // itself is spawned by claude from the --mcp-config file, so sgh never
    // owns an MCP server process.
    if mcp.is_some() {
        allowed_tools.push_str(",mcp__topodb");
    }

    argv.push("--allowedTools".to_string());
    argv.push(allowed_tools);

    argv.push("--output-format".to_string());
    argv.push("json".to_string());

    if let Some(m) = mcp {
        argv.push("--mcp-config".to_string());
        argv.push(m.config_path.clone());
    }

    if let Some(m) = model {
        argv.push("--model".to_string());
        argv.push(m);
    }

    argv
}

/// Decide what a completed `claude -p` invocation actually accomplished.
///
/// Exit status alone cannot answer this. When a tool call is blocked, `claude`
/// reports `subtype: "success"`, `is_error: false`, and exits 0 — the denial
/// appears only in `permission_denials`. A node whose Write was denied changed
/// nothing, so trusting the exit code records a no-op as completed work, and a
/// run that produced no output becomes indistinguishable from one that did the
/// whole job.
///
/// `expects_json` is true when the node declares an `output.schema`. The model
/// is told to reply with bare JSON, but intermittently wraps it in a ```json
/// fence or a sentence of prose even so. When JSON is expected, unwrap that
/// wrapping (`extract_json`) so a spurious formatting deviation is not treated
/// as a failed node; schema validation downstream still enforces correctness.
/// A reply containing no JSON object is left untouched and fails there,
/// honestly. When JSON is not expected (e.g. a survey node returning prose),
/// the result is never altered.
pub fn interpret_result(stdout: &str, expects_json: bool) -> NodeOutcome {
    let v: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => {
            // Unreadable output is not evidence of work. Failing here is the
            // conservative reading: it surfaces a broken invocation instead of
            // passing an unexamined string downstream as though it were a
            // result.
            return NodeOutcome::Failed {
                error: format!(
                    "claude produced unparseable output ({e}): {}",
                    elide(stdout)
                ),
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
        return NodeOutcome::Denied { tool: denied.join(", ") };
    }

    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false) {
        let detail = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
        return NodeOutcome::Failed {
            error: format!("claude reported an error: {}", detail.trim()),
        };
    }

    match v.get("result").and_then(|r| r.as_str()) {
        Some(r) => {
            let trimmed = r.trim();
            let output = if expects_json {
                // Prefer unwrapped JSON; fall back to the raw reply so a
                // no-JSON response still fails at schema validation with the
                // reply visible, rather than being silently emptied here.
                extract_json(trimmed).unwrap_or_else(|| trimmed.to_string())
            } else {
                trimmed.to_string()
            };
            NodeOutcome::Succeeded { output }
        }
        None => NodeOutcome::Failed {
            error: format!("claude returned no `result` field: {}", elide(stdout)),
        },
    }
}

pub struct ClaudeCodeRunner {
    model: Option<String>,
    bash_grants: Vec<String>,
    mcp: Option<McpWiring>,
}

impl ClaudeCodeRunner {
    pub fn new(model: Option<String>, bash_grants: Vec<String>, mcp: Option<McpWiring>) -> Self {
        ClaudeCodeRunner {
            model,
            bash_grants,
            mcp,
        }
    }
}

impl AgentRunner for ClaudeCodeRunner {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
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
        //
        // Bash grants are additive on top of these ambient permissions: each
        // `Bash(prefix:*)` widens what an UNGATED agent prompt can execute.
        // The run-level gate echo (shown before approval) is the human control —
        // grants here alone do not confine or restrict agent execution.
        // MCP grants follow the same additive doctrine.
        let node_mcp = if req.tools.iter().any(|t| t == crate::schema::TOPODB_TOOL) {
            self.mcp.as_ref()
        } else {
            None
        };
        let argv = build_argv(
            build_prompt(req),
            self.model.clone(),
            &self.bash_grants,
            node_mcp,
        );
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);

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
        Ok(interpret_result(&stdout, req.output_schema.is_some()))
    }
}
