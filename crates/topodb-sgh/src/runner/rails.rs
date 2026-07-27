//! Character/shell rails for operator-supplied grant strings.

/// Shared character/shell rail behind [`validate_bash_grant`] and
/// [`validate_mcp_server_command`]. `what` names the value in error messages
/// ("bash grant prefix" / "agent-mcp server command"). A rail, not a security
/// boundary — see the callers' doc comments.
fn validate_rail(what: &str, value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{what} is empty or whitespace-only"));
    }
    for ch in &[';', '|', '&', '<', '>', '`', '$', ',', '(', ')', ':'] {
        if trimmed.contains(*ch) {
            return Err(format!(
                "{what} '{value}' contains forbidden character '{ch}'"
            ));
        }
    }
    let forbidden_shells = ["sh", "bash", "zsh", "dash", "ksh", "fish", "env"];
    for token in trimmed.split_whitespace() {
        let base_cmd = token
            .split('/')
            .next_back()
            .unwrap_or("")
            .to_ascii_lowercase();
        if forbidden_shells.contains(&base_cmd.as_str()) {
            return Err(format!(
                "{what} '{value}' contains a shell or generic launcher ({base_cmd}), not a binary"
            ));
        }
    }
    Ok(())
}

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
    validate_rail("bash grant prefix", prefix)
}

/// Validate and split an `--agent-mcp` server command into argv.
///
/// Same rail as bash grants (empty / metacharacters / shell basenames
/// rejected — including `:`, which also rules out Windows drive paths; the
/// bash-grant rail shares that limitation). The value is whitespace-split
/// with NO shell involved — the same no-shell doctrine as the plugin's goal
/// handling — so paths with spaces are unsupported, exactly as they are for
/// bash grants. First token is the server binary; prefer an absolute path
/// (same textual-honesty lesson as bash grants).
pub fn validate_mcp_server_command(cmd: &str) -> Result<Vec<String>, String> {
    validate_rail("agent-mcp server command", cmd)?;
    Ok(cmd.split_whitespace().map(String::from).collect())
}
