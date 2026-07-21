use std::str::FromStr;
use topodb::{Direction, EdgeRecord, NodeId, NodeRecord, PropValue, Scope, ScopeSet, Subgraph};

pub fn parse_scope(s: Option<&str>) -> napi::Result<Scope> {
    topodb_json::resolve_scope(s, Scope::Shared).map_err(crate::errors::rejected)
}

pub fn parse_scopes(scopes: &[String]) -> napi::Result<ScopeSet> {
    let mut resolved = Vec::with_capacity(scopes.len());
    for s in scopes {
        resolved.push(parse_scope(Some(s))?);
    }
    Ok(topodb_json::scopes_to_scope_set(&resolved))
}

pub fn parse_node_id(s: &str) -> napi::Result<NodeId> {
    NodeId::from_str(s).map_err(|e| crate::errors::rejected(format!("invalid node id {s:?}: {e}")))
}

pub fn node_to_value(n: &NodeRecord) -> napi::Result<serde_json::Value> {
    topodb_json::node_to_json(n).map_err(|e| crate::errors::rejected(e))
}

pub fn nodes_to_value(ns: Vec<NodeRecord>) -> napi::Result<serde_json::Value> {
    let vals: Result<Vec<_>, String> = ns.iter().map(topodb_json::node_to_json).collect();
    let arr = vals.map_err(crate::errors::rejected)?;
    Ok(serde_json::Value::Array(arr))
}

pub fn edges_to_value(es: Vec<EdgeRecord>) -> napi::Result<serde_json::Value> {
    let vals: Result<Vec<_>, String> = es.iter().map(topodb_json::edge_to_json).collect();
    let arr = vals.map_err(crate::errors::rejected)?;
    Ok(serde_json::Value::Array(arr))
}

pub fn subgraph_to_value(sg: &Subgraph) -> napi::Result<serde_json::Value> {
    topodb_json::subgraph_to_json(sg).map_err(|e| crate::errors::rejected(e))
}

pub fn json_to_prop_value(j: &serde_json::Value) -> napi::Result<PropValue> {
    topodb_json::json_to_prop_value(j).map_err(crate::errors::rejected)
}

pub fn parse_direction(s: &str) -> napi::Result<Direction> {
    match s.to_ascii_lowercase().as_str() {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        other => Err(crate::errors::rejected(format!("invalid direction {other:?}"))),
    }
}
