use crossbeam_channel::{Receiver, RecvTimeoutError};
use pyo3::prelude::*;
use std::sync::Mutex;
use std::time::Duration;
use topodb::ChangeEvent;

pub fn event_to_json(ev: &ChangeEvent) -> Result<serde_json::Value, String> {
    let op = serde_json::to_value(&*ev.op).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"seq": ev.seq, "op": op}))
}

#[pyclass]
pub struct Subscription {
    rx: Mutex<Option<Receiver<ChangeEvent>>>,
}

impl Subscription {
    pub fn new(rx: Receiver<ChangeEvent>) -> Self {
        Self { rx: Mutex::new(Some(rx)) }
    }
}

#[pymethods]
impl Subscription {
    /// Blocking next with optional timeout (seconds). None on timeout.
    /// GIL released while waiting.
    #[pyo3(signature = (timeout=None))]
    fn next(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Option<PyObject>> {
        let rx = self
            .rx
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| crate::errors::closed(py))?;
        let got = py.allow_threads(|| match timeout {
            Some(secs) => match rx.recv_timeout(Duration::from_secs_f64(secs)) {
                Ok(ev) => Ok(Some(ev)),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => Err(()),
            },
            None => rx.recv().map(Some).map_err(|_| ()),
        });
        match got {
            Ok(Some(ev)) => {
                let j = event_to_json(&ev).map_err(|e| crate::errors::rejected(py, e))?;
                Ok(Some(crate::convert::json_to_py(py, &j)?))
            }
            Ok(None) => Ok(None),
            Err(()) => Err(crate::errors::closed(py)),
        }
    }

    fn close(&self) {
        self.rx.lock().unwrap().take();
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<PyObject> {
        match self.next(py, None) {
            Ok(Some(ev)) => Ok(ev),
            Ok(None) => Err(pyo3::exceptions::PyStopIteration::new_err(())),
            Err(e) => {
                let closed = py.import("topodb.errors")?.getattr("ClosedError")?;
                if e.matches(py, &closed)? {
                    Err(pyo3::exceptions::PyStopIteration::new_err(()))
                } else {
                    Err(e)
                }
            }
        }
    }
}
