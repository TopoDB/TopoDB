//! Context warehouse: the bronze tier under `<db>.warehouse/` (see the design
//! spec `2026-08-18-context-warehouse-design.md`). Layer on the engine, no LLM.
pub mod blob;
pub mod event;
pub mod manifest;
pub mod mirror;
pub mod paths;
pub mod redact;
pub mod report;
pub mod segment;
pub mod spool;

pub use blob::{blob_path, get_blob, hash_hex, put_blob};
pub use event::{
    Artifact, ArtifactType, Event, Kind, Marker, MarkerType, OpEvent, Redaction, Source,
    EVENT_VERSION,
};
pub use manifest::{Manifest, MirrorGap, SegmentEntry, Tier, MANIFEST_VERSION, RECENT_IDS_CAP};
pub use mirror::{mirrored_seq, MirrorReport, MIRRORED_SEQ_KEY};
pub use paths::{warehouse_dir_for_db, Layout};
pub use report::Status;
pub use spool::DrainReport;

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

/// An open warehouse directory: layout + manifest + config.
pub struct Warehouse {
    pub layout: Layout,
    pub manifest: Manifest,
    pub config: WarehouseConfig,
}
impl Warehouse {
    /// Opens the warehouse, loading its manifest and seeding the dedup id set
    /// from any events already durable in the open segment. This closes the
    /// crash window between `segment::append_events` and `manifest::save`:
    /// if a crash occurs after events are appended but before the manifest is
    /// saved, the ids are recovered from the segment on re-open, preventing
    /// duplicates when those events are re-drained.
    pub fn open(dir: &std::path::Path, config: WarehouseConfig) -> std::io::Result<Self> {
        let layout = Layout::new(dir.to_path_buf());
        layout.ensure()?;
        let mut manifest = Manifest::load_or_init(&layout)?;
        // Close the crash window between segment append and manifest save:
        // any ids already durable in the open segment count as seen.
        if let Some(e) = manifest.open_entry().cloned() {
            let (evs, _) = segment::read_segment(&layout, &e)?;
            for ev in evs {
                manifest.note_id(&ev.id);
            }
        }
        Ok(Warehouse {
            layout,
            manifest,
            config,
        })
    }
    pub fn save(&self) -> std::io::Result<()> {
        self.manifest.save(&self.layout)
    }
    pub fn drain(&mut self, now_ms: i64) -> std::io::Result<DrainReport> {
        let r = spool::drain(&self.layout, &mut self.manifest, &self.config, now_ms)?;
        self.maybe_roll(now_ms)?;
        Ok(r)
    }
    /// Seals the open segment when it is over `segment_mb` or from a previous UTC day.
    pub fn maybe_roll(&mut self, now_ms: i64) -> std::io::Result<()> {
        let roll = self
            .manifest
            .open_entry()
            .is_some_and(|e| segment::should_roll(e, now_ms, self.config.segment_mb));
        if roll {
            segment::seal_open(&self.layout, &mut self.manifest)?;
            self.save()?;
        }
        Ok(())
    }
    pub fn events(&self) -> std::io::Result<Vec<Event>> {
        segment::all_events(&self.layout, &self.manifest)
    }
    pub fn mirror(&mut self, db: &topodb::Db, now_ms: i64) -> Result<MirrorReport, WarehouseError> {
        let r = mirror::mirror(db, &self.layout, &mut self.manifest, now_ms)?;
        self.maybe_roll(now_ms)?;
        Ok(r)
    }
}
