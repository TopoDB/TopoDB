//! Ingest and seed reporting.

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IngestReport {
    pub ingested: usize, // Created only; a supersession's new head counts under `superseded`
    pub superseded: usize,
    pub deduplicated: usize,
    pub skipped: usize, // unchanged + entity stubs
    pub errors: Vec<FileError>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileError {
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SeedReport {
    pub seeded: usize,
    pub stubs: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub errors: Vec<FileError>,
}
