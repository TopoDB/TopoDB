use thiserror::Error;

#[derive(Debug, Error)]
pub enum TopoError {
    #[error("storage error: {0}")]
    Storage(#[from] redb::Error),
    #[error("encoding error: {0}")]
    Encoding(String),
    #[error("batch rejected: {0}")]
    Rejected(String),
    #[error("op log compacted; oldest retained seq is {oldest}")]
    Compacted { oldest: u64 },
    #[error("database closed")]
    Closed,
}
