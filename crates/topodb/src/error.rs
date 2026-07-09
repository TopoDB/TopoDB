use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TopoError {
    #[error("storage error: {0}")]
    Storage(Box<redb::Error>),
    #[error("encoding error: {0}")]
    Encoding(String),
    #[error("batch rejected: {0}")]
    Rejected(String),
    #[error("op log compacted; oldest retained seq is {oldest}")]
    Compacted { oldest: u64 },
    #[error("database closed")]
    Closed,
    #[error("unsupported format version {found} (this build supports up to {supported})")]
    UnsupportedFormat { found: u32, supported: u32 },
}

impl From<redb::Error> for TopoError {
    fn from(e: redb::Error) -> Self {
        TopoError::Storage(Box::new(e))
    }
}

/// Converts any redb sub-error (`TableError`, `TransactionError`,
/// `StorageError`, ...) into a boxed [`TopoError::Storage`] in one step —
/// the call-site replacement for the old two-hop
/// `.map_err(redb::Error::from)?` (redb-suberror → `redb::Error` → `?`
/// via `#[from]`). `redb::Error` itself already implements `Into<redb::Error>`
/// (identity), so this also covers sites that already had a `redb::Error`.
pub(crate) fn storage_err(e: impl Into<redb::Error>) -> TopoError {
    TopoError::Storage(Box::new(e.into()))
}
