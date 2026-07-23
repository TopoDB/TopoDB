use std::process::Command;

use super::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};

/// Validate a bash grant prefix.
///
/// This is a rail to catch obviously problematic prefixes — not a security boundary.
/// Rejects:
/// - Empty or whitespace-only strings
/// - Every whitespace-separated token's basename (after `/`) if it matches
///   a shell command (case-insensitive) in {sh, bash, zsh, dash, ksh, fish, env}
/// - Any of the characters `;`, `|`, `&`, `<`, `>`, `` ` ``, `$`, `,`, `(`, `)`, `:`
///
/// Error message names the prefix and explains why it was rejected.
pub fn validate_bash_grant(prefix: &str) -> Result<(), String> {
    let trimmed = prefix.trim();

    // Reject empty or whitespace-only
    if trimmed.is_empty() {
        return Err("bash grant prefix is empty or whitespace-only".to_string());
    }

    // Reject rule-injection and metacharacters
    for ch in &[';', '|', '&', '<', '>', '`', '$', ',', '(', ')', ':'] {
        if trimmed.contains(*ch) {
            return Err(format!(
                "bash grant prefix '{}' contains forbidden character '{}'",
                prefix, ch
            ));
        }
    }

    // Shell set: case-insensitive match (bash is the most common, but zsh, sh,
    // dash, ksh, and fish are also shells; env is a generic launcher).
    let forbidden_shells = ["sh", "bash", "zsh", "dash", "ksh", "fish", "env"];

    // Check every whitespace-separated token's basename
    for token in trimmed.split_whitespace() {
        let base_cmd = token
            .split('/')
            .next_back()
            .unwrap_or("")
            .to_ascii_lowercase();

        if forbidden_shells.contains(&base_cmd.as_str()) {
            return Err(format!(
                "bash grant prefix '{}' contains a shell or generic launcher ({}), not a binary",
                prefix, base_cmd
            ));
        }
    }

    Ok(())
}

/// Build the command-line arguments for invoking `claude -p`.
///
/// Returns a vector of arguments suitable for `std::process::Command`.
/// Includes the prompt, allowedTools (with optional bash grants), output format,
/// and model if specified.
///
/// Structured output (--output-format json) is what makes a denied tool visible at all:
/// in plain-text mode a blocked tool call is indistinguishable from a completed one,
/// since both exit 0 with prose on stdout. This ensures that when a node's Write is
/// denied, we can detect it in the JSON response's permission_denials field.
pub fn build_argv(prompt: String, model: Option<String>, bash_grants: &[String]) -> Vec<String> {
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

    argv.push("--allowedTools".to_string());
    argv.push(allowed_tools);

    argv.push("--output-format".to_string());
    argv.push("json".to_string());

    if let Some(m) = model {
        argv.push("--model".to_string());
        argv.push(m);
    }

    argv
}

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
             no code fences. Output that does not match is treated as a failure. \
             Even if you find the work already done and change nothing, still reply \
             with JSON reflecting the current state (e.g. counts of what already \
             exists) — never an explanation instead of the JSON.\n\n",
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

/// Pull a JSON object or array out of a model reply, tolerating the two most
/// common ways the model wraps it despite being told not to: a ```json …```
/// (or bare ```` ``` ````) fence, and one or more sentences of prose around
/// the object. Returns the JSON substring only if it actually parses; a stray
/// unbalanced brace in prose yields `None`, not a false positive. A reply that
/// is already bare JSON is returned unchanged.
pub fn extract_json(reply: &str) -> Option<String> {
    let s = reply.trim();

    // Whole reply already parses — the common, well-behaved case.
    if serde_json::from_str::<serde_json::Value>(s).is_ok() {
        return Some(s.to_string());
    }

    // A fenced block: ```json\n…\n``` or ```\n…\n```. Take the fence body.
    if let Some(after) = s.strip_prefix("```") {
        // Drop an optional language tag on the first line (e.g. `json`).
        let body = match after.find('\n') {
            Some(nl) => &after[nl + 1..],
            None => after,
        };
        let body = body.strip_suffix("```").unwrap_or(body).trim();
        if serde_json::from_str::<serde_json::Value>(body).is_ok() {
            return Some(body.to_string());
        }
    }

    // Prose around an object/array: scan for the first opening bracket and
    // find the balanced close by trying successive candidates. Cheap because
    // agent replies are short; correctness comes from requiring a real parse.
    let bytes = s.as_bytes();
    let open = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let close_char = if bytes[open] == b'{' { b'}' } else { b']' };
    // Search from the last matching close back toward `open` so the widest
    // balanced span is tried first.
    let mut end = s.len();
    while let Some(rel) = s[open..end].rfind(close_char as char) {
        let candidate = &s[open..open + rel + 1];
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
            return Some(candidate.to_string());
        }
        end = open + rel; // try a shorter span
    }
    None
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
    bash_grants: Vec<String>,
}

impl ClaudeCodeRunner {
    pub fn new(model: Option<String>, bash_grants: Vec<String>) -> Self {
        ClaudeCodeRunner { model, bash_grants }
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
        let argv = build_argv(build_prompt(req), self.model.clone(), &self.bash_grants);
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
