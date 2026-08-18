//! Context warehouse: the bronze tier under `<db>.warehouse/` (see the design
//! spec `2026-08-18-context-warehouse-design.md`). Layer on the engine, no LLM.
pub mod blob;
pub mod event;
pub mod manifest;
pub mod paths;

pub use blob::{blob_path, get_blob, hash_hex, put_blob};
pub use event::{
    Artifact, ArtifactType, Event, Kind, Marker, MarkerType, OpEvent, Redaction, Source,
    EVENT_VERSION,
};
pub use manifest::{Manifest, MirrorGap, SegmentEntry, Tier, MANIFEST_VERSION, RECENT_IDS_CAP};
pub use paths::{warehouse_dir_for_db, Layout};

/// Tunables (spec §8 `[warehouse]`), with the spec defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct WarehouseConfig {
    pub hot_days: u64,
    pub warm_days: u64,
    pub retention_days: u64,
    pub purge_expired: bool,
    pub segment_mb: u64,
    pub max_inline_kb: u64,
    pub max_artifact_kb: u64,
    pub redact: bool,
    pub evidence_k: usize,
    pub tier_batch: usize,
    /// Spool files younger than this (by mtime) are left for the next drain.
    pub spool_min_age_ms: u64,
}
impl Default for WarehouseConfig {
    fn default() -> Self {
        WarehouseConfig {
            hot_days: 14,
            warm_days: 180,
            retention_days: 730,
            purge_expired: false,
            segment_mb: 64,
            max_inline_kb: 16,
            max_artifact_kb: 512,
            redact: true,
            evidence_k: 20,
            tier_batch: 500,
            spool_min_age_ms: 2000,
        }
    }
}

#[derive(Debug)]
pub enum WarehouseError {
    Io(std::io::Error),
    Engine(topodb::TopoError),
    Invalid(String),
}
impl std::fmt::Display for WarehouseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WarehouseError::Io(e) => write!(f, "io: {e}"),
            WarehouseError::Engine(e) => write!(f, "engine: {e}"),
            WarehouseError::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}
impl std::error::Error for WarehouseError {}
impl From<std::io::Error> for WarehouseError {
    fn from(e: std::io::Error) -> Self {
        WarehouseError::Io(e)
    }
}
impl From<topodb::TopoError> for WarehouseError {
    fn from(e: topodb::TopoError) -> Self {
        WarehouseError::Engine(e)
    }
}
