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

    /// Bytes the engine has actually allocated to pages, where it can report
    /// that — as opposed to the size of the file on disk.
    ///
    /// These differ, and the difference is not a detail. TopoDB's redb file
    /// grows by *doubling* (`page_manager.rs`: `usable_bytes() * 2`), so its
    /// file size is quantized: 10k and 15k node corpora produce byte-identical
    /// files, as do 20k and 30k. File utilization therefore swings between
    /// ~59% just after a doubling and ~89% just before one, and a
    /// single-corpus-size file-bytes comparison inherits that swing wholesale
    /// — measured against minigraf, the same engines compare as 1.53x at 20k
    /// nodes but 1.02-1.03x at 15k and 30k.
    ///
    /// Engines that cannot report allocation return `None` and are compared
    /// on file bytes alone, with the caveat stated in the report.
    fn allocated_bytes(_path: &Path) -> Result<Option<u64>, EngineError> {
        Ok(None)
    }

    fn as_of_support() -> AsOfSupport;
}
