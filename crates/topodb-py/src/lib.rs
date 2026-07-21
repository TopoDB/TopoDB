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
