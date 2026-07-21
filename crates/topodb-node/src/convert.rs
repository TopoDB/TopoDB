use std::str::FromStr;
use topodb::{NodeId, Scope, ScopeSet};

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
