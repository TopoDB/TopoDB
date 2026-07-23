use thiserror::Error;

#[derive(Debug, Error)]
pub enum TopoError {
    #[error("storage error: {0}")]
    Storage(Box<redb::Error>),
    /// The database file is exclusively locked by another live handle
    /// (usually another process). Transient by nature — callers may retry
    /// with backoff; every other error variant is not retryable this way.
    #[error("database is held by another process (lock not acquired)")]
    Busy,
    #[error("encoding error: {0}")]
    Encoding(String),
    /// An invalid request, rejected before anything was committed. Raised by
    /// both write paths (a bad op in a batch) and read paths (e.g. querying a
    /// prop that isn't equality-indexed), so the message stays neutral — the
    /// inner string says what was actually wrong.
    #[error("rejected: {0}")]
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

/// Maps a redb open failure: lock contention becomes the typed, retryable
/// [`TopoError::Busy`]; everything else stays a storage error. Every call
/// site that opens the underlying redb database must use this (there are
/// two: `open_with_options` and `read_persisted_index_spec`) so a held
/// file is `Busy` no matter which open path reaches it first.
pub(crate) fn open_err(e: redb::DatabaseError) -> TopoError {
    match e {
        redb::DatabaseError::DatabaseAlreadyOpen => TopoError::Busy,
        other => storage_err(other),
    }
}
