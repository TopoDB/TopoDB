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
