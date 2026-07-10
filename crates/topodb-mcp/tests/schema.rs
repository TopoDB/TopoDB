//! Schema-shape tests for the advertised `tools/list` surface.
//!
//! These guard a failure mode the handler-level tests structurally cannot
//! catch. `e2e.rs`/`plan6.rs` build their arguments with `serde_json::json!`
//! in-process and hand a real `Value` straight to the tool body, so they pass
//! regardless of what the *schema* claims. A real MCP client has only the
//! schema to go on: when a param renders as a typeless (permissive) schema —
//! which is what `serde_json::Value` emits on its own — a client has no
//! signal that the param wants an array or an object, and may encode it as a
//! JSON string. The server then rejects it (`expected a JSON array of
//! numbers`, `expected a JSON object for props`, ...) and the tool is
//! unusable over the wire while every in-process test still passes.
//!
//! So: assert on what the client actually sees.

mod common;

use common::{expect_tool_error, structured_content, Server, DEFAULT_TIMEOUT};
use serde_json::json;

/// Spawns a server on a throwaway db and returns its `tools/list` array.
fn tools() -> (tempfile::TempDir, Vec<serde_json::Value>) {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("schema.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);
    let tools = server.tools_list(DEFAULT_TIMEOUT);
    (dir, tools)
}

/// The `inputSchema.properties` map for `tool_name`.
fn properties(tools: &[serde_json::Value], tool_name: &str) -> serde_json::Value {
    let tool = tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(tool_name))
        .unwrap_or_else(|| panic!("tools/list should expose {tool_name}"));
    tool.get("inputSchema")
        .and_then(|s| s.get("properties"))
        .unwrap_or_else(|| panic!("{tool_name} should have inputSchema.properties: {tool:#?}"))
        .clone()
}

/// Whether a property subschema tells a client what JSON type to encode.
/// `type` is the direct answer; `$ref`/`enum`/`anyOf`/`oneOf`/`allOf` each
/// delegate to a subschema that carries one.
fn declares_a_type(prop: &serde_json::Value) -> bool {
    ["type", "$ref", "enum", "anyOf", "oneOf", "allOf"]
        .iter()
        .any(|k| prop.get(k).is_some())
}

/// Collects the JSON type names a property permits, ignoring a `"null"`
/// alternative contributed by `Option<T>`.
fn type_names(prop: &serde_json::Value) -> Vec<String> {
    match prop.get("type") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(xs)) => xs
            .iter()
            .filter_map(|x| x.as_str())
            .filter(|s| *s != "null")
            .map(str::to_string)
            .collect(),
        _ => vec![],
    }
}

/// The regression this file exists for: every structured param must declare
/// the JSON type a client is supposed to encode. Before the fix each of these
/// was a bare `serde_json::Value`, rendering as `{"description": "..."}` with
/// no `type` at all.
#[test]
fn structured_params_declare_their_json_type() {
    let (_dir, tools) = tools();

    // (tool, param, expected JSON type)
    let cases = [
        ("create_memory", "props", "object"),
        ("create_entity", "props", "object"),
        ("link", "props", "object"),
        ("set_node_props", "props", "object"),
        ("set_embedding", "vector", "array"),
        ("search_vectors", "vector", "array"),
        ("submit_batch", "commands", "array"),
    ];

    for (tool, param, expected) in cases {
        let props = properties(&tools, tool);
        let schema = props
            .get(param)
            .unwrap_or_else(|| panic!("{tool} should have a {param} param: {props:#?}"));
        assert!(
            declares_a_type(schema),
            "{tool}.{param} declares no JSON type — MCP clients cannot tell how to \
             encode it and may send a string: {schema:#?}"
        );
        assert_eq!(
            type_names(schema),
            vec![expected.to_string()],
            "{tool}.{param} should advertise type {expected:?}: {schema:#?}"
        );
    }
}

/// `find_by_prop`'s `value` is the one case that can fail *silently*: a string
/// value round-trips fine, so a stringified integer would simply match nothing
/// instead of erroring. The schema must spell out the scalar union so a client
/// keeps `1815` an integer.
#[test]
fn find_by_prop_value_advertises_the_scalar_union() {
    let (_dir, tools) = tools();
    let props = properties(&tools, "find_by_prop");
    let schema = props
        .get("value")
        .expect("find_by_prop should have `value`");

    assert!(
        declares_a_type(schema),
        "find_by_prop.value declares no JSON type: {schema:#?}"
    );
    let mut got = type_names(schema);
    got.sort();
    assert_eq!(
        got,
        vec![
            "boolean".to_string(),
            "integer".to_string(),
            "string".to_string()
        ],
        "find_by_prop.value should advertise the equality-indexable scalars \
         (floats are not indexable): {schema:#?}"
    );
}

/// Demonstrates *why* `find_by_prop.value` needed a spelled-out schema rather
/// than just "some type": an integer-valued prop looked up with a stringified
/// integer does not error — it returns zero rows. A client that guesses the
/// encoding gets a plausible, wrong, silent answer. Every other typeless param
/// failed loudly; this one didn't.
///
/// Also exercises a real `props` object end-to-end over the wire, which is the
/// encoding the fixed `{"type": "object"}` schema now asks clients for.
#[test]
fn stringified_integer_value_silently_matches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let spec_path = dir.path().join("spec.json");
    std::fs::write(
        &spec_path,
        r#"{"equality":[{"label":"Entity","prop":"name"},{"label":"Entity","prop":"born"}],"text":[]}"#,
    )
    .unwrap();

    let mut server = Server::spawn(
        &dir.path().join("typing.redb"),
        &["--spec", spec_path.to_str().unwrap()],
    );
    server.initialize(DEFAULT_TIMEOUT);

    server.call_tool_ok(
        "create_entity",
        json!({ "name": "Ada", "props": { "born": 1815 } }),
        DEFAULT_TIMEOUT,
    );

    // The correct encoding: an integer stays an integer, and matches.
    let hit = server.call_tool_ok(
        "find_by_prop",
        json!({ "label": "Entity", "prop": "born", "value": 1815 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        hit["nodes"].as_array().map(Vec::len),
        Some(1),
        "integer 1815 should match the indexed integer prop: {hit:#?}"
    );

    // The hazard: `"1815"` is itself a legal `value`, so this is not an error —
    // it is a successful lookup that finds nothing.
    let resp = server.call_tool(
        "find_by_prop",
        json!({ "label": "Entity", "prop": "born", "value": "1815" }),
        DEFAULT_TIMEOUT,
    );
    let miss = structured_content(&resp);
    assert_eq!(
        miss["nodes"].as_array().map(Vec::len),
        Some(0),
        "a stringified integer should match nothing (silently) — this is the \
         failure mode the `value` schema exists to prevent: {miss:#?}"
    );
}

/// A numeric bound the server enforces must also be *advertised*, or a client
/// will happily send a value that is always rejected. `max_hops` is rejected
/// outside `1..=4` (not clamped), and both `k`s are rejected at 0 — yet the
/// derived schemas said `minimum: 0` / `maximum: 255` (a bare `u8`/`usize`
/// range). Each case asserts the advertised bound *and* that the runtime
/// really rejects just outside it, so the two can't drift apart.
#[test]
fn numeric_bounds_match_the_runtime_contract() {
    let (_dir, tools) = tools();

    let hops = properties(&tools, "traverse");
    let hops = &hops["max_hops"];
    assert_eq!(
        hops["minimum"],
        json!(1),
        "traverse.max_hops min: {hops:#?}"
    );
    assert_eq!(
        hops["maximum"],
        json!(4),
        "traverse.max_hops max: {hops:#?}"
    );

    for tool in ["search_memories", "search_vectors"] {
        let props = properties(&tools, tool);
        let k = &props["k"];
        assert_eq!(
            k["minimum"],
            json!(1),
            "{tool}.k should advertise min 1: {k:#?}"
        );
    }

    // And the runtime genuinely rejects just outside those bounds.
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(&dir.path().join("bounds.redb"), &[]);
    server.initialize(DEFAULT_TIMEOUT);
    let seed = server.call_tool_ok("create_entity", json!({ "name": "A" }), DEFAULT_TIMEOUT)["id"]
        .as_str()
        .expect("create_entity returns an id")
        .to_string();

    for bad in [0u8, 5u8] {
        let resp = server.call_tool(
            "traverse",
            json!({ "seed_id": seed, "max_hops": bad }),
            DEFAULT_TIMEOUT,
        );
        expect_tool_error(&resp);
    }

    let resp = server.call_tool(
        "search_memories",
        json!({ "query": "anything", "k": 0 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);

    let resp = server.call_tool(
        "search_vectors",
        json!({ "model": "m", "vector": [1.0], "k": 0 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
}

/// Blanket invariant so a future tool can't reintroduce a typeless param: no
/// property of any tool's input schema may be left without a type.
#[test]
fn no_tool_param_is_typeless() {
    let (_dir, tools) = tools();

    let mut offenders = vec![];
    for tool in &tools {
        let name = tool
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("<unnamed>");
        let Some(props) = tool.get("inputSchema").and_then(|s| s.get("properties")) else {
            continue; // A no-arg tool (db_info) may omit `properties` entirely.
        };
        let Some(props) = props.as_object() else {
            continue;
        };
        for (param, schema) in props {
            if !declares_a_type(schema) {
                offenders.push(format!("{name}.{param}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these params advertise no JSON type, so MCP clients must guess how to \
         encode them: {offenders:#?}"
    );
}
