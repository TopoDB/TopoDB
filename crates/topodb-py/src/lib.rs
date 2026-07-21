mod convert;
mod errors;

use pyo3::prelude::*;
use std::sync::Mutex;
use topodb::Db;

#[pyclass]
pub struct TopoDB {
    inner: Mutex<Option<Db>>,
}

impl TopoDB {
    /// Db is Clone around Arc<Inner>; clone-or-ClosedError.
    fn db(&self, py: Python<'_>) -> PyResult<Db> {
        self.inner
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| errors::closed(py))
    }
}

#[pymethods]
impl TopoDB {
    #[staticmethod]
    fn open(py: Python<'_>, path: String) -> PyResult<Self> {
        let db = py
            .allow_threads(|| Db::open(&path))
            .map_err(|e| errors::to_py(py, e))?;
        Ok(Self { inner: Mutex::new(Some(db)) })
    }

    #[staticmethod]
    fn open_with(py: Python<'_>, path: String, index_spec: &Bound<'_, PyAny>) -> PyResult<Self> {
        let spec = convert::parse_index_spec(py, index_spec)?;
        let db = py
            .allow_threads(|| Db::open_with(&path, spec))
            .map_err(|e| errors::to_py(py, e))?;
        Ok(Self { inner: Mutex::new(Some(db)) })
    }

    fn format_version(&self, py: Python<'_>) -> PyResult<u32> {
        Ok(self.db(py)?.format_version())
    }

    fn close(&self) {
        self.inner.lock().unwrap().take();
    }

    #[pyo3(signature = (commands, default_scope=None, now_ms=None))]
    fn submit(
        &self,
        py: Python<'_>,
        commands: &Bound<'_, PyAny>,
        default_scope: Option<&str>,
        now_ms: Option<i64>,
    ) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let batch = convert::py_to_json(commands)?;
        let scope = convert::parse_scope(py, default_scope)?;
        let (ops, ids) = topodb_json::resolve_batch(&batch, scope)
            .map_err(|e| errors::rejected(py, e))?;
        let applied = py
            .allow_threads(|| match now_ms {
                Some(t) => db.submit_at(ops, t),
                None => db.submit(ops),
            })
            .map_err(|e| errors::to_py(py, e))?;
        convert::json_to_py(
            py,
            &serde_json::json!({
                "first_seq": applied.first_seq,
                "last_seq": applied.last_seq,
                "ids": ids,
            }),
        )
    }

    fn node(&self, py: Python<'_>, scopes: Vec<String>, id: &str) -> PyResult<Option<PyObject>> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let nid = convert::parse_node_id(py, id)?;
        py.allow_threads(|| db.node(&set, nid))
            .map(|n| convert::node_to_py(py, &n))
            .transpose()
    }

    fn nodes_by_label(&self, py: Python<'_>, scopes: Vec<String>, label: &str) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let nodes = py.allow_threads(|| db.nodes_by_label(&set, label));
        convert::nodes_to_py(py, nodes)
    }

    fn nodes_by_label_newest(&self, py: Python<'_>, scopes: Vec<String>, label: &str, k: usize) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let nodes = py.allow_threads(|| db.nodes_by_label_newest(&set, label, k));
        convert::nodes_to_py(py, nodes)
    }

    fn nodes_by_prop(&self, py: Python<'_>, scopes: Vec<String>, label: &str, prop: &str, value: &Bound<'_, PyAny>) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let pv = convert::py_to_prop_value(value)?;
        let nodes = py
            .allow_threads(|| db.nodes_by_prop(&set, label, prop, &pv))
            .map_err(|e| errors::to_py(py, e))?;
        convert::nodes_to_py(py, nodes)
    }

    fn nodes_by_prop_normalized(&self, py: Python<'_>, scopes: Vec<String>, label: &str, prop: &str, value: &Bound<'_, PyAny>) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let pv = convert::py_to_prop_value(value)?;
        let nodes = py
            .allow_threads(|| db.nodes_by_prop_normalized(&set, label, prop, &pv))
            .map_err(|e| errors::to_py(py, e))?;
        convert::nodes_to_py(py, nodes)
    }

    fn nodes_by_float_range(&self, py: Python<'_>, scopes: Vec<String>, prop: &str, min: f64, max: f64) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let nodes = py.allow_threads(|| db.nodes_by_float_range(&set, prop, min, max));
        convert::nodes_to_py(py, nodes)
    }

    #[pyo3(signature = (scopes, from_, to=None, r#type=None, open_only=false))]
    fn edges_from(&self, py: Python<'_>, scopes: Vec<String>, from_: &str, to: Option<&str>, r#type: Option<&str>, open_only: bool) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let from_id = convert::parse_node_id(py, from_)?;
        let to_id = to.map(|s| convert::parse_node_id(py, s)).transpose()?;
        let edges = py
            .allow_threads(|| db.edges_from(&set, from_id, to_id, r#type, open_only))
            .map_err(|e| errors::to_py(py, e))?;
        convert::edges_to_py(py, edges)
    }

    fn all_edges_between(&self, py: Python<'_>, from_: &str, to: &str) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let from_id = convert::parse_node_id(py, from_)?;
        let to_id = convert::parse_node_id(py, to)?;
        let edges = py.allow_threads(|| db.all_edges_between(from_id, to_id));
        convert::edges_to_py(py, edges)
    }

    fn open_edges_between(&self, py: Python<'_>, from_: &str, to: &str) -> PyResult<Vec<String>> {
        let db = self.db(py)?;
        let from_id = convert::parse_node_id(py, from_)?;
        let to_id = convert::parse_node_id(py, to)?;
        let edge_ids = py.allow_threads(|| db.open_edges_between(from_id, to_id));
        Ok(edge_ids.iter().map(|e| e.to_string()).collect())
    }

    #[pyo3(signature = (scopes, seeds, max_hops, edge_types=None, direction="both", as_of=None))]
    fn traverse(
        &self,
        py: Python<'_>,
        scopes: Vec<String>,
        seeds: Vec<String>,
        max_hops: u8,
        edge_types: Option<Vec<String>>,
        direction: &str,
        as_of: Option<i64>,
    ) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let q = topodb::TraversalQuery {
            scopes: convert::parse_scopes(py, scopes)?,
            seeds: seeds.iter().map(|s| convert::parse_node_id(py, s)).collect::<PyResult<_>>()?,
            max_hops,
            edge_types: edge_types.map(|ts| ts.into_iter().map(Into::into).collect()),
            direction: convert::parse_direction(py, direction)?,
            as_of,
        };
        let sg = py.allow_threads(|| db.traverse(&q)).map_err(|e| errors::to_py(py, e))?;
        convert::subgraph_to_py(py, &sg)
    }

    #[pyo3(signature = (scopes, query, k, recency_weight=0.0, recency_half_life_ms=0, now_ms=None))]
    fn search_text(
        &self,
        py: Python<'_>,
        scopes: Vec<String>,
        query: String,
        k: usize,
        recency_weight: f32,
        recency_half_life_ms: i64,
        now_ms: Option<i64>,
    ) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let set = convert::parse_scopes(py, scopes)?;
        let options = topodb::SearchOptions {
            recency_weight,
            recency_half_life_ms,
            now_ms,
            ..Default::default()
        };
        let hits = py
            .allow_threads(|| db.search_text_with(&set, &query, k, &options))
            .map_err(|e| errors::to_py(py, e))?;
        convert::scored_to_py(py, hits)
    }

    #[pyo3(signature = (scopes, model, vector, k, candidates=None))]
    fn search_vector(
        &self,
        py: Python<'_>,
        scopes: Vec<String>,
        model: String,
        vector: Vec<f32>,
        k: usize,
        candidates: Option<Vec<String>>,
    ) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let q = topodb::VectorQuery {
            scopes: convert::parse_scopes(py, scopes)?,
            model,
            vector,
            k,
            candidates: candidates
                .map(|cs| cs.iter().map(|s| convert::parse_node_id(py, s)).collect::<PyResult<_>>())
                .transpose()?,
        };
        let hits = py.allow_threads(|| db.search_vector(&q)).map_err(|e| errors::to_py(py, e))?;
        convert::scored_to_py(py, hits)
    }

    #[pyo3(signature = (scopes, query, k, vector=None, expansions=None, graph_boost=false, labels=None, now_ms=None))]
    fn recall(
        &self,
        py: Python<'_>,
        scopes: Vec<String>,
        query: String,
        k: usize,
        vector: Option<(String, Vec<f32>)>,
        expansions: Option<Vec<(String, Vec<String>)>>,
        graph_boost: bool,
        labels: Option<Vec<String>>,
        now_ms: Option<i64>,
    ) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let mut q = topodb::RecallQuery::new(convert::parse_scopes(py, scopes)?, query, k);
        q.vector = vector;
        q.expansions = expansions.unwrap_or_default();
        q.graph_boost = graph_boost;
        q.labels = labels;
        q.options.now_ms = now_ms;
        let hits = py.allow_threads(|| db.recall(&q)).map_err(|e| errors::to_py(py, e))?;
        convert::scored_to_py(py, hits)
    }

    #[pyo3(signature = (scopes, node, k, model=None, as_of=None, min_semantic_similarity=None))]
    fn suggest_links(
        &self,
        py: Python<'_>,
        scopes: Vec<String>,
        node: &str,
        k: usize,
        model: Option<String>,
        as_of: Option<i64>,
        min_semantic_similarity: Option<f32>,
    ) -> PyResult<PyObject> {
        let db = self.db(py)?;
        let q = topodb::SuggestLinksQuery {
            scopes: convert::parse_scopes(py, scopes)?,
            node: convert::parse_node_id(py, node)?,
            k,
            model,
            as_of,
            min_semantic_similarity,
        };
        let out = py.allow_threads(|| db.suggest_links(&q)).map_err(|e| errors::to_py(py, e))?;
        let mut rows = Vec::with_capacity(out.len());
        for s in &out {
            let node = topodb_json::node_to_json(&s.node).map_err(|e| errors::rejected(py, e))?;
            rows.push(serde_json::json!({
                "node": node,
                "score": s.score,
                "common_neighbors": s.common_neighbors.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                "structural": s.structural,
                "semantic": s.semantic,
            }));
        }
        convert::json_to_py(py, &serde_json::Value::Array(rows))
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) -> bool {
        self.close();
        false
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<TopoDB>()?;
    Ok(())
}
