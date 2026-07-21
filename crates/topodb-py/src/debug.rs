use pyo3::prelude::*;
use topodb::Db;

/// Unstable debug surface for dumping all nodes.
/// This API is subject to change and not part of the stable v1 interface.
pub fn dump_nodes(py: Python<'_>, db: &Db) -> PyResult<PyObject> {
    let nodes = py.allow_threads(|| db.debug_dump_nodes());
    crate::convert::nodes_to_py(py, nodes)
}

/// Unstable debug surface for dumping all edges.
/// This API is subject to change and not part of the stable v1 interface.
pub fn dump_edges(py: Python<'_>, db: &Db) -> PyResult<PyObject> {
    let edges = py.allow_threads(|| db.debug_dump_edges());
    crate::convert::edges_to_py(py, edges)
}
