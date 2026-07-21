use pyo3::prelude::*;
use std::str::FromStr;
use topodb::{Direction, EdgeRecord, IndexSpec, NodeId, NodeRecord, Scope, ScopeSet};
use topodb::{PropValue, Subgraph};

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

pub fn node_to_py(py: Python<'_>, n: &NodeRecord) -> PyResult<PyObject> {
    json_to_py(py, &topodb_json::node_to_json(n).map_err(|e| crate::errors::rejected(py, e))?)
}

pub fn nodes_to_py(py: Python<'_>, ns: Vec<NodeRecord>) -> PyResult<PyObject> {
    let vals: Result<Vec<_>, String> = ns.iter().map(topodb_json::node_to_json).collect();
    json_to_py(py, &serde_json::Value::Array(vals.map_err(|e| crate::errors::rejected(py, e))?))
}

pub fn edges_to_py(py: Python<'_>, es: Vec<EdgeRecord>) -> PyResult<PyObject> {
    let vals: Result<Vec<_>, String> = es.iter().map(topodb_json::edge_to_json).collect();
    json_to_py(py, &serde_json::Value::Array(vals.map_err(|e| crate::errors::rejected(py, e))?))
}

pub fn subgraph_to_py(py: Python<'_>, sg: &Subgraph) -> PyResult<PyObject> {
    json_to_py(py, &topodb_json::subgraph_to_json(sg).map_err(|e| crate::errors::rejected(py, e))?)
}

pub fn py_to_prop_value(v: &Bound<'_, PyAny>) -> PyResult<PropValue> {
    let j = py_to_json(v)?;
    topodb_json::json_to_prop_value(&j).map_err(|e| crate::errors::rejected(v.py(), e))
}

pub fn parse_direction(py: Python<'_>, s: &str) -> PyResult<Direction> {
    match s.to_ascii_lowercase().as_str() {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        other => Err(crate::errors::rejected(py, format!("invalid direction {other:?}"))),
    }
}

pub fn parse_index_spec(py: Python<'_>, spec: &Bound<'_, PyAny>) -> PyResult<IndexSpec> {
    let j = py_to_json(spec)?;
    serde_json::from_value(j).map_err(|e| crate::errors::rejected(py, format!("invalid index spec: {e}")))
}

pub fn scored_to_py(py: Python<'_>, hits: Vec<(NodeRecord, f32)>) -> PyResult<PyObject> {
    let mut rows = Vec::with_capacity(hits.len());
    for (n, score) in hits {
        let node = topodb_json::node_to_json(&n).map_err(|e| crate::errors::rejected(py, e))?;
        rows.push(serde_json::json!({
            "node": node,
            "score": score,
        }));
    }
    json_to_py(py, &serde_json::Value::Array(rows))
}
