use pyo3::prelude::*;
use std::str::FromStr;
use topodb::{NodeId, Scope, ScopeSet};

pub fn py_to_json(v: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    pythonize::depythonize(v).map_err(|e| crate::errors::rejected(v.py(), e))
}

pub fn json_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<PyObject> {
    Ok(pythonize::pythonize(py, v)
        .map_err(|e| crate::errors::rejected(py, e))?
        .into())
}

pub fn parse_scope(py: Python<'_>, s: Option<&str>) -> PyResult<Scope> {
    topodb_json::resolve_scope(s, Scope::Shared).map_err(|e| crate::errors::rejected(py, e))
}

pub fn parse_scopes(py: Python<'_>, scopes: Vec<String>) -> PyResult<ScopeSet> {
    let mut resolved = Vec::with_capacity(scopes.len());
    for s in &scopes {
        resolved.push(parse_scope(py, Some(s))?);
    }
    Ok(topodb_json::scopes_to_scope_set(&resolved))
}

pub fn parse_node_id(py: Python<'_>, s: &str) -> PyResult<NodeId> {
    NodeId::from_str(s).map_err(|e| crate::errors::rejected(py, format!("invalid node id {s:?}: {e}")))
}
