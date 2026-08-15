//! Ingest and seed reporting. Same shape as `topodb-obsidian`'s reports so the
//! CLI/MCP surface stays uniform.

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IngestReport {
    /// Concept pages created (a fresh concept, or a dangling stub upgraded to
    /// its first real content).
    pub ingested: usize,
    /// Pages whose body changed: the old memory is superseded, history kept.
    pub superseded: usize,
    /// Reserved for content-identical duplicates (kept for report symmetry).
    pub deduplicated: usize,
    /// Unchanged pages on re-ingest.
    pub skipped: usize,
    pub errors: Vec<FileError>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileError {
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SeedReport {
    /// Concept pages written (new or changed on disk).
    pub seeded: usize,
    /// Reserved files (`index.md`/`log.md`) written.
    pub reserved: usize,
    /// Pages/files already identical on disk.
    pub unchanged: usize,
    /// Pages skipped to preserve a local edit (non-clobber, no `overwrite`).
    pub skipped: usize,
    pub errors: Vec<FileError>,
}
