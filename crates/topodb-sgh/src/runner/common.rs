//! Provider-neutral helpers shared by every AgentRunner family.

use super::NodeRequest;

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
pub fn elide(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    let head: String = s.chars().take(200).collect();
    format!("{head}…")
}
