//! The comparable surface. Both drivers implement exactly this, so neither can
//! quietly do less work than the other.

use std::path::Path;

use crate::corpus::Corpus;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine error: {0}")]
    Backend(String),
}

/// What a point lookup returns. Deliberately small and identical for both
/// engines so neither is timed materialising more than the other.
#[derive(Debug, Clone, PartialEq)]
pub struct Payload {
    pub name: String,
    pub rank: i64,
}

/// Whether an engine can answer "what was this node's payload at time T".
///
/// Reported rather than worked around: TopoDB cannot today, and presenting an
/// edge-validity traversal as the same operation would make the benchmark lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsOfSupport {
    /// Historical node payloads are queryable.
    Supported,
    /// The engine has temporal edges but cannot return a historical payload.
    NodePayloadUnsupported,
}

pub trait Engine: Sized {
    fn open(path: &Path) -> Result<Self, EngineError>;

    /// Load the whole corpus. Timed as the insert benchmark.
    fn insert_corpus(&mut self, corpus: &Corpus) -> Result<(), EngineError>;

    /// Fetch one node's payload by logical id.
    fn point_lookup(&self, id: usize) -> Result<Option<Payload>, EngineError>;

    /// Count distinct nodes reachable from `seed` within `depth` hops.
    fn k_hop(&self, seed: usize, depth: u8) -> Result<usize, EngineError>;

    fn on_disk_bytes(&self) -> Result<u64, EngineError>;

    fn as_of_support() -> AsOfSupport;
}
