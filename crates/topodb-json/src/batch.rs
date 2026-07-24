//! The batch-submit DSL: a JSON array of high-level commands (each an object
//! keyed by `"op"`, whose value equals the corresponding MCP tool name) that
//! both `topodb-cli submit` and `topodb-mcp submit_batch` consume. Turns the
//! array into a `Vec<Op>` for one atomic `Db::submit`, resolving `#N`
//! back-references (0-indexed: `#0` is the first command) to ULIDs produced by
//! earlier commands in the same batch.
//!
//! Supported ops: `create_memory`, `create_entity`, `create_node` (an
//! arbitrary-label node, for host-level schemas like episode recording),
//! `link`, `set_node_props`, `remove_node`, `close_edge`, `set_embedding`.

use crate::{
    json_to_f32_vec, json_to_prop_changes, json_to_props, merge_required_prop, resolve_scope,
    ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP, MEMORY_LABEL,
};
use serde_json::Value;
use std::str::FromStr;
use topodb::{EdgeId, NodeId, Op, PropValue, Props, Scope};

#[derive(Clone, Copy, PartialEq, Eq)]
enum IdKind {
    Node,
    Edge,
}

fn kind_name(k: IdKind) -> &'static str {
    match k {
        IdKind::Node => "node",
        IdKind::Edge => "edge",
    }
}

/// Pulls a required string field, naming the command index on failure.
fn req_str(obj: &serde_json::Map<String, Value>, key: &str, idx: usize) -> Result<String, String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("command #{idx}: missing string field {key:?}"))
}

/// An optional i64 field: absent → None; present-and-integer → Some; present
/// -but-not-integer → Err.
fn opt_i64(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    idx: usize,
) -> Result<Option<i64>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("command #{idx}: {key:?} must be an integer")),
    }
}

/// Resolves a command's `scope` field (create_memory/create_entity/link all
/// carry one): absent → the batch default; a string → parsed; non-string →
/// Err.
fn scope_of(
    obj: &serde_json::Map<String, Value>,
    default: Scope,
    idx: usize,
) -> Result<Scope, String> {
    match obj.get("scope") {
        None | Some(Value::Null) => Ok(default),
        Some(Value::String(s)) => {
            resolve_scope(Some(s), default).map_err(|e| format!("command #{idx}: {e}"))
        }
        Some(_) => Err(format!("command #{idx}: \"scope\" must be a string")),
    }
}

/// Resolves an id-bearing field to a ULID string: a literal ULID passes through
/// verbatim; `#N` (0-indexed: `#0` is the first command) resolves to command N's
/// produced id, requiring N < idx (backward-only) and a matching id kind (node vs
/// edge).
fn resolve_ref(
    raw: &str,
    expected: IdKind,
    produced: &[Option<(String, IdKind)>],
    field: &str,
    idx: usize,
) -> Result<String, String> {
    let Some(rest) = raw.strip_prefix('#') else {
        return Ok(raw.to_string());
    };
    let n: usize = rest
        .parse()
        .map_err(|_| format!("command #{idx}: {field} back-ref {raw:?} is not #<number>"))?;
    if n >= idx {
        return Err(format!(
            "command #{idx}: {field} back-ref {raw:?} must point to an earlier command"
        ));
    }
    match &produced[n] {
        Some((id, kind)) if *kind == expected => Ok(id.clone()),
        Some((_, kind)) => Err(format!(
            "command #{idx}: {field} back-ref #{n} refers to a {} id but a {} id is required here",
            kind_name(*kind),
            kind_name(expected)
        )),
        None => Err(format!(
            "command #{idx}: {field} back-ref #{n} refers to a command that produces no id"
        )),
    }
}

fn parse_node(s: &str, field: &str, idx: usize) -> Result<NodeId, String> {
    NodeId::from_str(s).map_err(|e| format!("command #{idx}: invalid {field} node id {s:?}: {e}"))
}

fn parse_edge(s: &str, field: &str, idx: usize) -> Result<EdgeId, String> {
    EdgeId::from_str(s).map_err(|e| format!("command #{idx}: invalid {field} edge id {s:?}: {e}"))
}

pub fn resolve_batch(
    batch: &Value,
    default_scope: Scope,
) -> Result<(Vec<Op>, Vec<Option<String>>), String> {
    let arr = batch
        .as_array()
        .ok_or_else(|| "batch must be a JSON array of command objects".to_string())?;
    let mut produced: Vec<Option<(String, IdKind)>> = Vec::with_capacity(arr.len());
    let mut ops: Vec<Op> = Vec::with_capacity(arr.len());

    for (idx, cmd) in arr.iter().enumerate() {
        let obj = cmd
            .as_object()
            .ok_or_else(|| format!("command #{idx}: expected a JSON object"))?;
        let op_name = obj
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("command #{idx}: missing string field \"op\""))?;

        match op_name {
            "create_memory" => {
                let content = req_str(obj, "content", idx)?;
                let scope = scope_of(obj, default_scope, idx)?;
                let props = merge_required_prop(
                    MEMORY_CONTENT_PROP,
                    PropValue::Str(content),
                    obj.get("props"),
                )
                .map_err(|e| format!("command #{idx}: {e}"))?;
                let id = NodeId::new();
                produced.push(Some((id.to_string(), IdKind::Node)));
                ops.push(Op::CreateNode {
                    id,
                    scope,
                    label: MEMORY_LABEL.into(),
                    props,
                });
            }
            "create_entity" => {
                let name = req_str(obj, "name", idx)?;
                let scope = scope_of(obj, default_scope, idx)?;
                let props =
                    merge_required_prop(ENTITY_NAME_PROP, PropValue::Str(name), obj.get("props"))
                        .map_err(|e| format!("command #{idx}: {e}"))?;
                let id = NodeId::new();
                produced.push(Some((id.to_string(), IdKind::Node)));
                ops.push(Op::CreateNode {
                    id,
                    scope,
                    label: ENTITY_LABEL.into(),
                    props,
                });
            }
            "create_node" => {
                let label = req_str(obj, "label", idx)?;
                if label.is_empty() {
                    return Err(format!("command #{idx}: \"label\" must be non-empty"));
                }
                if label == ENTITY_LABEL {
                    return Err(format!(
                        "command #{idx}: label {ENTITY_LABEL:?} is reserved — use the create_entity command"
                    ));
                }
                if label == MEMORY_LABEL {
                    return Err(format!(
                        "command #{idx}: label {MEMORY_LABEL:?} is reserved — use the create_memory command"
                    ));
                }
                let scope = scope_of(obj, default_scope, idx)?;
                let props = match obj.get("props") {
                    Some(v) => json_to_props(v).map_err(|e| format!("command #{idx}: {e}"))?,
                    None => Props::new(),
                };
                let id = NodeId::new();
                produced.push(Some((id.to_string(), IdKind::Node)));
                ops.push(Op::CreateNode {
                    id,
                    scope,
                    label: label.into(),
                    props,
                });
            }
            "link" => {
                let from_raw = req_str(obj, "from", idx)?;
                let to_raw = req_str(obj, "to", idx)?;
                let ty = crate::normalize_edge_type(&req_str(obj, "type", idx)?)
                    .map_err(|e| format!("command #{idx}: {e}"))?;
                let scope = scope_of(obj, default_scope, idx)?;
                let from = parse_node(
                    &resolve_ref(&from_raw, IdKind::Node, &produced, "from", idx)?,
                    "from",
                    idx,
                )?;
                let to = parse_node(
                    &resolve_ref(&to_raw, IdKind::Node, &produced, "to", idx)?,
                    "to",
                    idx,
                )?;
                let props = match obj.get("props") {
                    Some(v) => json_to_props(v).map_err(|e| format!("command #{idx}: {e}"))?,
                    None => Props::new(),
                };
                let valid_from = opt_i64(obj, "valid_from", idx)?;
                let id = EdgeId::new();
                produced.push(Some((id.to_string(), IdKind::Edge)));
                ops.push(Op::CreateEdge {
                    id,
                    scope,
                    ty: ty.into(),
                    from,
                    to,
                    props,
                    valid_from,
                });
            }
            "set_node_props" => {
                let id_raw = req_str(obj, "id", idx)?;
                let id = parse_node(
                    &resolve_ref(&id_raw, IdKind::Node, &produced, "id", idx)?,
                    "id",
                    idx,
                )?;
                let props_val = obj
                    .get("props")
                    .ok_or_else(|| format!("command #{idx}: set_node_props requires \"props\""))?;
                let props =
                    json_to_prop_changes(props_val).map_err(|e| format!("command #{idx}: {e}"))?;
                produced.push(None);
                ops.push(Op::SetNodeProps { id, props });
            }
            "remove_node" => {
                let id_raw = req_str(obj, "id", idx)?;
                let id = parse_node(
                    &resolve_ref(&id_raw, IdKind::Node, &produced, "id", idx)?,
                    "id",
                    idx,
                )?;
                produced.push(None);
                ops.push(Op::RemoveNode { id });
            }
            "close_edge" => {
                let id_raw = req_str(obj, "id", idx)?;
                let id = parse_edge(
                    &resolve_ref(&id_raw, IdKind::Edge, &produced, "id", idx)?,
                    "id",
                    idx,
                )?;
                let valid_to = opt_i64(obj, "valid_to", idx)?;
                produced.push(None);
                ops.push(Op::CloseEdge { id, valid_to });
            }
            "set_embedding" => {
                let id_raw = req_str(obj, "id", idx)?;
                let id = parse_node(
                    &resolve_ref(&id_raw, IdKind::Node, &produced, "id", idx)?,
                    "id",
                    idx,
                )?;
                let model = req_str(obj, "model", idx)?;
                let vector_val = obj
                    .get("vector")
                    .ok_or_else(|| format!("command #{idx}: set_embedding requires \"vector\""))?;
                let vector =
                    json_to_f32_vec(vector_val).map_err(|e| format!("command #{idx}: {e}"))?;
                produced.push(None);
                ops.push(Op::SetEmbedding { id, model, vector });
            }
            other => return Err(format!(
                "command #{idx}: unknown op {other:?} (ops use underscore names like \"create_entity\" — not the CLI's hyphenated command names)"
            )),
        }
    }

    let produced_ids = produced.into_iter().map(|p| p.map(|(id, _)| id)).collect();
    Ok((ops, produced_ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use topodb::ScopeId;

    fn ids(ops: &[Op]) -> Vec<String> {
        ops.iter()
            .map(|op| match op {
                Op::CreateNode { id, .. } => id.to_string(),
                Op::CreateEdge { id, .. } => id.to_string(),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn create_and_backref_link_resolves_to_generated_ids() {
        let batch = serde_json::json!([
            { "op": "create_entity", "name": "Ada" },
            { "op": "create_memory", "content": "met Ada" },
            { "op": "link", "from": "#1", "to": "#0", "type": "about" }
        ]);
        let (ops, produced) = resolve_batch(&batch, Scope::Shared).unwrap();
        assert_eq!(ops.len(), 3);
        let node_ids = ids(&ops);
        // link's from == command #1's node id, to == command #0's node id.
        match &ops[2] {
            Op::CreateEdge { from, to, ty, .. } => {
                assert_eq!(from.to_string(), node_ids[1]);
                assert_eq!(to.to_string(), node_ids[0]);
                assert_eq!(ty.as_str(), "about");
            }
            other => panic!("expected CreateEdge, got {other:?}"),
        }
        // produced ids: node, node, edge.
        assert_eq!(produced[0].as_deref(), Some(node_ids[0].as_str()));
        assert_eq!(produced[1].as_deref(), Some(node_ids[1].as_str()));
        assert!(produced[2].is_some());
    }

    #[test]
    fn set_node_props_null_removes_key() {
        let batch = serde_json::json!([
            { "op": "set_node_props", "id": "00000000000000000000000000",
              "props": { "stale": null, "status": "x" } }
        ]);
        let (ops, produced) = resolve_batch(&batch, Scope::Shared).unwrap();
        match &ops[0] {
            Op::SetNodeProps { props, .. } => {
                assert_eq!(props["stale"], None);
                assert_eq!(props["status"], Some(PropValue::Str("x".into())));
            }
            other => panic!("expected SetNodeProps, got {other:?}"),
        }
        assert_eq!(produced[0], None);
    }

    #[test]
    fn forward_reference_is_rejected() {
        let batch = serde_json::json!([
            { "op": "link", "from": "#1", "to": "#1", "type": "x" },
            { "op": "create_entity", "name": "late" }
        ]);
        let err = resolve_batch(&batch, Scope::Shared).unwrap_err();
        assert!(err.contains("earlier"), "got: {err}");
    }

    #[test]
    fn node_slot_rejects_edge_backref() {
        // command #0 is a link (produces an EDGE id); using #0 in link's
        // `from` (a NODE slot) must be rejected on kind.
        let batch = serde_json::json!([
            { "op": "create_entity", "name": "a" },
            { "op": "create_entity", "name": "b" },
            { "op": "link", "from": "#0", "to": "#1", "type": "x" },
            { "op": "link", "from": "#2", "to": "#0", "type": "y" }
        ]);
        let err = resolve_batch(&batch, Scope::Shared).unwrap_err();
        assert!(err.contains("edge") && err.contains("node"), "got: {err}");
    }

    #[test]
    fn close_edge_backref_to_in_batch_link_is_allowed() {
        let batch = serde_json::json!([
            { "op": "create_entity", "name": "a" },
            { "op": "create_entity", "name": "b" },
            { "op": "link", "from": "#0", "to": "#1", "type": "x" },
            { "op": "close_edge", "id": "#2" }
        ]);
        let (ops, _) = resolve_batch(&batch, Scope::Shared).unwrap();
        let edge_id = match &ops[2] {
            Op::CreateEdge { id, .. } => id.to_string(),
            _ => unreachable!(),
        };
        match &ops[3] {
            Op::CloseEdge { id, valid_to } => {
                assert_eq!(id.to_string(), edge_id);
                assert_eq!(*valid_to, None); // applier fills "now"
            }
            other => panic!("expected CloseEdge, got {other:?}"),
        }
    }

    #[test]
    fn unknown_op_names_the_index() {
        let batch = serde_json::json!([{ "op": "nope" }]);
        let err = resolve_batch(&batch, Scope::Shared).unwrap_err();
        assert!(err.contains("#0") && err.contains("nope"), "got: {err}");
    }

    #[test]
    fn unknown_op_error_includes_underscore_hint() {
        let batch = serde_json::json!([{ "op": "create-entity" }]);
        let err = resolve_batch(&batch, Scope::Shared).unwrap_err();
        assert!(
            err.contains("underscore"),
            "error should hint about underscore names: {err}"
        );
    }

    #[test]
    fn non_array_batch_is_rejected() {
        let err = resolve_batch(&serde_json::json!({"op": "x"}), Scope::Shared).unwrap_err();
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn set_embedding_parses_vector() {
        let batch = serde_json::json!([
            { "op": "set_embedding", "id": "00000000000000000000000000",
              "model": "m", "vector": [0.1, 0.2, 0.3] }
        ]);
        let (ops, _) = resolve_batch(&batch, Scope::Shared).unwrap();
        match &ops[0] {
            Op::SetEmbedding { model, vector, .. } => {
                assert_eq!(model, "m");
                assert_eq!(vector.len(), 3);
            }
            other => panic!("expected SetEmbedding, got {other:?}"),
        }
    }

    #[test]
    fn create_node_with_arbitrary_label_props_and_backref() {
        let batch = serde_json::json!([
            { "op": "create_node", "label": "Episode",
              "props": { "goal": "fix the bug", "turns": 3 } },
            { "op": "create_node", "label": "RetrievalEvent",
              "props": { "query": "bug history" } },
            { "op": "link", "from": "#0", "to": "#1", "type": "ISSUED" }
        ]);
        let (ops, ids) = resolve_batch(&batch, Scope::Shared).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(ids[0].is_some() && ids[1].is_some() && ids[2].is_some());
        match &ops[0] {
            Op::CreateNode { label, props, .. } => {
                assert_eq!(label, "Episode");
                assert_eq!(
                    props.get("goal"),
                    Some(&PropValue::Str("fix the bug".into()))
                );
                assert_eq!(props.get("turns"), Some(&PropValue::Int(3)));
            }
            other => panic!("expected CreateNode, got {other:?}"),
        }
        match &ops[2] {
            Op::CreateEdge { from, to, .. } => {
                assert_eq!(from.to_string(), ids[0].clone().unwrap());
                assert_eq!(to.to_string(), ids[1].clone().unwrap());
            }
            other => panic!("expected CreateEdge, got {other:?}"),
        }
    }

    #[test]
    fn link_honours_an_explicit_scope() {
        let other = ScopeId::new();
        let batch = serde_json::json!([
            { "op": "create_entity", "name": "a" },
            { "op": "create_entity", "name": "b" },
            { "op": "link", "from": "#0", "to": "#1", "type": "x", "scope": other.to_string() }
        ]);
        // Default scope is Shared; the link must land in `other`, NOT the default.
        let (ops, _produced) = resolve_batch(&batch, Scope::Shared).unwrap();
        match &ops[2] {
            Op::CreateEdge { scope, .. } => assert_eq!(*scope, Scope::Id(other)),
            other_op => panic!("expected CreateEdge, got {other_op:?}"),
        }
    }

    #[test]
    fn create_node_requires_nonempty_label() {
        for batch in [
            serde_json::json!([{ "op": "create_node" }]),
            serde_json::json!([{ "op": "create_node", "label": "" }]),
        ] {
            assert!(resolve_batch(&batch, Scope::Shared).is_err());
        }
    }

    #[test]
    fn create_node_rejects_reserved_labels() {
        for (label, dedicated) in [("Entity", "create_entity"), ("Memory", "create_memory")] {
            let batch = serde_json::json!([{ "op": "create_node", "label": label }]);
            let err = resolve_batch(&batch, Scope::Shared).unwrap_err();
            assert!(
                err.contains("#0") && err.contains("reserved") && err.contains(dedicated),
                "label {label:?}: got: {err}"
            );
        }
    }

    #[test]
    fn create_node_scope_field_is_honored() {
        let batch = serde_json::json!([
            { "op": "create_node", "label": "Harness", "scope": "shared" }
        ]);
        let (ops, _) = resolve_batch(&batch, Scope::Id(topodb::ScopeId::new())).unwrap();
        match &ops[0] {
            Op::CreateNode { scope, .. } => assert_eq!(*scope, Scope::Shared),
            other => panic!("expected CreateNode, got {other:?}"),
        }
    }

    #[test]
    fn link_without_scope_falls_back_to_the_default() {
        let default_id = ScopeId::new();
        let batch = serde_json::json!([
            { "op": "create_entity", "name": "a" },
            { "op": "create_entity", "name": "b" },
            { "op": "link", "from": "#0", "to": "#1", "type": "x" }
        ]);
        let (ops, _produced) = resolve_batch(&batch, Scope::Id(default_id)).unwrap();
        match &ops[2] {
            Op::CreateEdge { scope, .. } => assert_eq!(*scope, Scope::Id(default_id)),
            other_op => panic!("expected CreateEdge, got {other_op:?}"),
        }
    }
}
