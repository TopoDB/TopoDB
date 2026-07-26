//! Obsidian-format vault ⇄ TopoDB transforms. Deterministic, no LLM.
//! One note = one memory; `topodb-id` in frontmatter is the identity key.

mod ingest;
mod mapping;
mod note;
mod report;
mod seed;
mod vault;
mod wikilink;

pub use ingest::{ingest_vault, plan_note, IngestOutcome, NoteAction};
pub use mapping::{note_to_input, NoteInput};
pub use note::Note;
pub use report::{FileError, IngestReport, SeedReport};
pub use seed::{
    render_entity_stub, render_memory_note, seed_vault, select_by_entity, select_by_query, slug,
};
pub use vault::{stamp_id, walk_vault, write_note};
pub use wikilink::extract_wikilinks;

/// Frontmatter identity key. Stamped by ingest; present on seeded notes.
pub const TOPODB_ID_KEY: &str = "topodb-id";
/// Seed's entity-link list. Becomes edges only — never a prop (fixpoint).
pub const RELATED_KEY: &str = "related";
/// Marks a seeded entity stub. Ingest skips these.
pub const ENTITY_STUB_KEY: &str = "entity";
/// Optional explicit title prop; seed uses it for filenames.
pub const TITLE_PROP: &str = "title";
