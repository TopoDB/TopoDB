use pyo3::prelude::*;
use topodb::TopoError;

fn cls<'py>(py: Python<'py>, name: &str) -> Bound<'py, PyAny> {
    py.import("topodb.errors")
        .expect("topodb.errors importable")
        .getattr(name)
        .expect("error class exists")
}

fn raise_arity1<'a>(py: Python<'a>, name: &str, arg: impl IntoPyObject<'a>) -> PyErr {
    match cls(py, name).call1((arg,)) {
        Ok(v) => PyErr::from_value(v),
        Err(e) => e,
    }
}

fn raise_arity2<'a>(py: Python<'a>, name: &str, arg1: impl IntoPyObject<'a>, arg2: impl IntoPyObject<'a>) -> PyErr {
    match cls(py, name).call1((arg1, arg2)) {
        Ok(v) => PyErr::from_value(v),
        Err(e) => e,
    }
}

fn raise_arity3<'a>(py: Python<'a>, name: &str, arg1: impl IntoPyObject<'a>, arg2: impl IntoPyObject<'a>, arg3: impl IntoPyObject<'a>) -> PyErr {
    match cls(py, name).call1((arg1, arg2, arg3)) {
        Ok(v) => PyErr::from_value(v),
        Err(e) => e,
    }
}

/// Exhaustive by construction — no wildcard arm. A new TopoError variant
/// fails this build, not silently maps to "unknown".
pub fn to_py(py: Python<'_>, e: TopoError) -> PyErr {
    let msg = e.to_string();
    match e {
        TopoError::Storage(_) => raise_arity1(py, "StorageError", msg),
        TopoError::Encoding(_) => raise_arity1(py, "EncodingError", msg),
        TopoError::Rejected(_) => raise_arity1(py, "RejectedError", msg),
        TopoError::Compacted { oldest } => raise_arity2(py, "CompactedError", msg, oldest),
        TopoError::Closed => raise_arity1(py, "ClosedError", msg),
        TopoError::UnsupportedFormat { found, supported } => {
            raise_arity3(py, "UnsupportedFormatError", msg, found, supported)
        }
    }
}

pub fn rejected(py: Python<'_>, msg: impl std::fmt::Display) -> PyErr {
    raise_arity1(py, "RejectedError", msg.to_string())
}

pub fn closed(py: Python<'_>) -> PyErr {
    raise_arity1(py, "ClosedError", "database closed".to_string())
}
