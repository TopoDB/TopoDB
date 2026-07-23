use std::collections::BTreeMap;

use topodb_sgh::runner::claude::{
    build_argv, build_prompt, extract_json, interpret_result, validate_bash_grant,
};
use topodb_sgh::runner::{NodeOutcome, NodeRequest};

#[test]
fn prompt_contains_only_declared_inputs() {
    let mut inputs = BTreeMap::new();
    inputs.insert("survey".to_string(), r#"{"sites":[1]}"#.to_string());

    let req = NodeRequest {
        node_id: "build".into(),
        prompt: "Apply the edits".into(),
        inputs,
        output_schema: None,
    };

    let p = build_prompt(&req);
    assert!(p.contains("Apply the edits"));
    assert!(p.contains("survey"));
    assert!(p.contains(r#"{"sites":[1]}"#));
}

#[test]
fn prompt_demands_bare_json_when_a_schema_is_declared() {
    let req = NodeRequest {
        node_id: "a".into(),
        prompt: "Do the thing".into(),
        inputs: BTreeMap::new(),
        output_schema: Some(serde_json::json!({"type": "object"})),
    };

    let p = build_prompt(&req);
    assert!(
        p.contains("JSON"),
        "schema-bearing nodes must be told to emit JSON"
    );
    assert!(p.contains(r#""type""#), "the schema itself is included");
}

#[test]
fn prompt_omits_the_input_section_when_there_are_none() {
    let req = NodeRequest {
        node_id: "a".into(),
        prompt: "Start here".into(),
        inputs: BTreeMap::new(),
        output_schema: None,
    };
    let p = build_prompt(&req);
    assert!(!p.contains("## Inputs"));
}

// --- interpret_result -------------------------------------------------------
//
// `claude -p --output-format json` reports a blocked tool call in
// `permission_denials` while every other field still claims success:
// `subtype` stays "success", `is_error` stays false, and the process exits 0.
// A node whose Write was denied changed nothing, so treating that as success
// records a no-op as completed work.

fn denied_write() -> &'static str {
    r#"{
        "subtype": "success",
        "is_error": false,
        "result": "The Write tool call was blocked.",
        "permission_denials": [
            {"tool_name": "Write", "tool_use_id": "toolu_01", "tool_input": {"file_path": "/tmp/x"}}
        ]
    }"#
}

#[test]
fn a_denied_tool_is_a_failure_even_though_claude_reports_success() {
    match interpret_result(denied_write(), false) {
        NodeOutcome::Failed { .. } => {}
        NodeOutcome::Succeeded { output } => {
            panic!("a node whose Write was denied did no work, got success: {output}")
        }
    }
}

#[test]
fn a_denial_failure_names_the_tool_that_was_blocked() {
    match interpret_result(denied_write(), false) {
        NodeOutcome::Failed { error } => assert!(
            error.contains("Write"),
            "the error must name the denied tool so the cause is diagnosable, got: {error}"
        ),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn a_clean_run_yields_the_result_field_not_the_raw_json() {
    let json = r#"{
        "subtype": "success",
        "is_error": false,
        "result": "PONG",
        "permission_denials": []
    }"#;
    assert_eq!(
        interpret_result(json, false),
        NodeOutcome::Succeeded {
            output: "PONG".to_string()
        }
    );
}

#[test]
fn an_api_error_is_a_failure() {
    let json = r#"{
        "subtype": "error_during_execution",
        "is_error": true,
        "result": "overloaded",
        "permission_denials": []
    }"#;
    match interpret_result(json, false) {
        NodeOutcome::Failed { .. } => {}
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn unparseable_output_is_a_failure_rather_than_silent_success() {
    match interpret_result("not json at all", false) {
        NodeOutcome::Failed { .. } => {}
        other => panic!("unreadable output proves nothing about the work, got {other:?}"),
    }
}

// --- extract_json + expects_json ---------------------------------------------
//
// A schema-bearing agent node must reply with JSON. In practice the model
// intermittently wraps that JSON in a ```json fence or a sentence of prose,
// even when told not to — observed directly: the same node that emitted bare
// `{"tests_added":7,...}` on one call narrated prose on a re-run and blocked.
// Unwrapping the two common deviations turns a spurious block into a success;
// a reply with no JSON object at all still fails honestly.

fn envelope(result_json_escaped: &str) -> String {
    format!(
        r#"{{"subtype":"success","is_error":false,"permission_denials":[],"result":{}}}"#,
        result_json_escaped
    )
}

#[test]
fn extract_json_returns_bare_object_unchanged() {
    assert_eq!(extract_json(r#"{"a":1}"#).as_deref(), Some(r#"{"a":1}"#));
}

#[test]
fn extract_json_unwraps_a_fenced_block() {
    let s = "```json\n{\"a\":1}\n```";
    assert_eq!(extract_json(s).as_deref(), Some("{\"a\":1}"));
}

#[test]
fn extract_json_pulls_an_object_out_of_surrounding_prose() {
    let s = "The work is already complete. {\"tests_added\": 7} — nothing else to do.";
    assert_eq!(extract_json(s).as_deref(), Some("{\"tests_added\": 7}"));
}

#[test]
fn extract_json_finds_nothing_in_pure_prose() {
    assert_eq!(
        extract_json("Everything looks fine, no changes needed."),
        None
    );
}

#[test]
fn extract_json_ignores_an_unbalanced_brace() {
    // A stray '{' with no valid object must not be mistaken for JSON.
    assert_eq!(extract_json("cost was ${5 and rising"), None);
}

#[test]
fn interpret_result_unwraps_wrapped_json_when_json_is_expected() {
    let result =
        serde_json::to_string("Here is the result:\n```json\n{\"tests_added\":7}\n```").unwrap();
    let env = envelope(&result);
    assert_eq!(
        interpret_result(&env, true),
        NodeOutcome::Succeeded {
            output: "{\"tests_added\":7}".to_string()
        },
        "wrapped JSON from a schema node must be unwrapped, not blocked"
    );
}

#[test]
fn interpret_result_leaves_prose_alone_when_no_json_is_expected() {
    // A node with no output schema legitimately returns prose (e.g. a survey).
    // It must never be mangled by extraction.
    let result = serde_json::to_string("A prose summary { with a brace } inside.").unwrap();
    let env = envelope(&result);
    assert_eq!(
        interpret_result(&env, false),
        NodeOutcome::Succeeded {
            output: "A prose summary { with a brace } inside.".to_string()
        }
    );
}

// --- build_argv ----------------------------------------------------------
//
// Tests for argv construction with bash grants.

#[test]
fn build_argv_with_no_grants_includes_base_allowed_tools() {
    let argv = build_argv("Test prompt".to_string(), None, &[]);
    let idx = argv
        .iter()
        .position(|arg| arg == "--allowedTools")
        .expect("--allowedTools not found");
    assert_eq!(argv[idx + 1], "Read,Write,Edit");
}

#[test]
fn build_argv_with_single_grant_appends_bash_grant() {
    let argv = build_argv("Test prompt".to_string(), None, &["topodb".to_string()]);
    let idx = argv
        .iter()
        .position(|arg| arg == "--allowedTools")
        .expect("--allowedTools not found");
    assert_eq!(argv[idx + 1], "Read,Write,Edit,Bash(topodb:*)");
}

#[test]
fn build_argv_with_multiple_grants_appends_all_in_order() {
    let argv = build_argv(
        "Test prompt".to_string(),
        None,
        &["topodb".to_string(), "cargo".to_string()],
    );
    let idx = argv
        .iter()
        .position(|arg| arg == "--allowedTools")
        .expect("--allowedTools not found");
    assert_eq!(
        argv[idx + 1],
        "Read,Write,Edit,Bash(topodb:*),Bash(cargo:*)"
    );
}

#[test]
fn build_argv_with_model_includes_model_flag() {
    let argv = build_argv(
        "Test prompt".to_string(),
        Some("claude-opus".to_string()),
        &[],
    );
    let has_model = argv
        .iter()
        .position(|arg| arg == "--model")
        .map(|idx| argv.get(idx + 1).map(|v| v.as_str()) == Some("claude-opus"))
        .unwrap_or(false);
    assert!(
        has_model,
        "argv should include --model flag with the given model"
    );
}

#[test]
fn build_argv_includes_base_flags() {
    let argv = build_argv("Test prompt".to_string(), None, &[]);
    assert!(
        argv.iter().any(|arg| arg == "-p"),
        "argv should include -p flag"
    );
    assert!(
        argv.iter().any(|arg| arg == "--output-format"),
        "argv should include --output-format flag"
    );
    let idx = argv
        .iter()
        .position(|arg| arg == "--output-format")
        .expect("--output-format not found");
    assert_eq!(
        argv[idx + 1],
        "json",
        "argv should have json as output-format value"
    );
}

#[test]
fn build_argv_full_order_empty_grants_with_model() {
    let argv = build_argv(
        "Do the thing".to_string(),
        Some("claude-opus".to_string()),
        &[],
    );
    assert_eq!(
        argv,
        vec![
            "claude",
            "-p",
            "Do the thing",
            "--allowedTools",
            "Read,Write,Edit",
            "--output-format",
            "json",
            "--model",
            "claude-opus",
        ]
    );
}

#[test]
fn build_argv_full_order_one_grant_no_model() {
    let argv = build_argv("Fix the code".to_string(), None, &["topodb".to_string()]);
    assert_eq!(
        argv,
        vec![
            "claude",
            "-p",
            "Fix the code",
            "--allowedTools",
            "Read,Write,Edit,Bash(topodb:*)",
            "--output-format",
            "json",
        ]
    );
}

// --- validate_bash_grant --------------------------------------------------
//
// Tests for bash grant validation.

#[test]
fn validate_bash_grant_accepts_simple_command() {
    assert_eq!(validate_bash_grant("topodb"), Ok(()));
}

#[test]
fn validate_bash_grant_rejects_empty_string() {
    let result = validate_bash_grant("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty"));
}

#[test]
fn validate_bash_grant_rejects_whitespace_only() {
    let result = validate_bash_grant("   ");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty"));
}

#[test]
fn validate_bash_grant_rejects_bash_shell() {
    let result = validate_bash_grant("bash");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("bash"));
}

#[test]
fn validate_bash_grant_rejects_sh_shell() {
    let result = validate_bash_grant("sh");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("sh"));
}

#[test]
fn validate_bash_grant_rejects_zsh_shell() {
    let result = validate_bash_grant("zsh");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("zsh"));
}

#[test]
fn validate_bash_grant_rejects_env_command() {
    let result = validate_bash_grant("env");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("env"));
}

#[test]
fn validate_bash_grant_rejects_bash_with_path() {
    let result = validate_bash_grant("/bin/bash");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("bash"));
}

#[test]
fn validate_bash_grant_rejects_semicolon() {
    let result = validate_bash_grant("a;b");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("a;b"));
}

#[test]
fn validate_bash_grant_rejects_pipe() {
    let result = validate_bash_grant("x | y");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("x | y"));
}

#[test]
fn validate_bash_grant_rejects_backtick() {
    let result = validate_bash_grant("`bash`");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("`bash`"));
}

#[test]
fn validate_bash_grant_rejects_dollar_paren() {
    let result = validate_bash_grant("$(x)");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("$(x)"));
}

#[test]
fn validate_bash_grant_rejects_ampersand() {
    let result = validate_bash_grant("x & y");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("x & y"));
}

#[test]
fn validate_bash_grant_rejects_redirect_in() {
    let result = validate_bash_grant("x < y");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("x < y"));
}

#[test]
fn validate_bash_grant_rejects_redirect_out() {
    let result = validate_bash_grant("x > y");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("x > y"));
}

#[test]
fn validate_bash_grant_rejects_dollar_expansion() {
    let result = validate_bash_grant("$var");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("$var"));
}

// --- Rule injection: reject metacharacters that enable multiple rules

#[test]
fn validate_bash_grant_rejects_comma() {
    let result = validate_bash_grant("a,b");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("a,b"));
}

#[test]
fn validate_bash_grant_rejects_open_paren() {
    let result = validate_bash_grant("a(b");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("a(b"));
}

#[test]
fn validate_bash_grant_rejects_close_paren() {
    let result = validate_bash_grant("a)b");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("a)b"));
}

#[test]
fn validate_bash_grant_rejects_colon() {
    let result = validate_bash_grant("a:b");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("a:b"));
}

#[test]
fn validate_bash_grant_rejects_complex_injection_attempt() {
    let result = validate_bash_grant("x:*),Bash(rm");
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("x:*),Bash(rm") || msg.contains("forbidden"));
}

// --- Case-insensitive shell denylist

#[test]
fn validate_bash_grant_rejects_bash_uppercase() {
    let result = validate_bash_grant("BASH");
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(err_msg.contains("bash") || err_msg.contains("BASH"));
}

#[test]
fn validate_bash_grant_rejects_bash_mixed_case() {
    let result = validate_bash_grant("Bash");
    assert!(result.is_err());
}

#[test]
fn validate_bash_grant_rejects_sh_uppercase() {
    let result = validate_bash_grant("SH");
    assert!(result.is_err());
}

#[test]
fn validate_bash_grant_rejects_zsh_mixed_case() {
    let result = validate_bash_grant("Zsh");
    assert!(result.is_err());
}

#[test]
fn validate_bash_grant_rejects_dash_shell() {
    let result = validate_bash_grant("dash");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("dash"));
}

#[test]
fn validate_bash_grant_rejects_ksh_shell() {
    let result = validate_bash_grant("ksh");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("ksh"));
}

#[test]
fn validate_bash_grant_rejects_fish_shell() {
    let result = validate_bash_grant("fish");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("fish"));
}

// --- All tokens checked, not just the first

#[test]
fn validate_bash_grant_rejects_bash_as_wrapped_command() {
    let result = validate_bash_grant("nice bash");
    assert!(result.is_err());
}

#[test]
fn validate_bash_grant_rejects_bash_after_timeout() {
    let result = validate_bash_grant("timeout 5 bash");
    assert!(result.is_err());
}

#[test]
fn validate_bash_grant_rejects_bash_after_xargs() {
    let result = validate_bash_grant("xargs bash");
    assert!(result.is_err());
}

#[test]
fn validate_bash_grant_accepts_topodb_search() {
    let result = validate_bash_grant("topodb search");
    assert!(
        result.is_ok(),
        "topodb search should be allowed (neither token is a shell)"
    );
}
