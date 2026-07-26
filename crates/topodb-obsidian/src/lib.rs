//! Obsidian-format vault ⇄ TopoDB transforms. Deterministic, no LLM.
//! One note = one memory; `topodb-id` in frontmatter is the identity key.

mod ingest; // Task 6-7
mod mapping;
mod note;
mod report; // Task 6
mod vault; // Task 5
mod wikilink; // Task 4
              // mod seed; // Task 8-9

pub use ingest::{ingest_vault, plan_note, IngestOutcome, NoteAction}; // Task 6-7
pub use mapping::{note_to_input, NoteInput}; // Task 4
pub use note::Note;
pub use report::{FileError, IngestReport, SeedReport}; // Task 6
                                                       // pub use seed::{
                                                       //     render_entity_stub, render_memory_note, seed_vault, select_by_entity, select_by_query, slug,
                                                       // }; // Task 8-9
pub use vault::{stamp_id, walk_vault, write_note}; // Task 5
pub use wikilink::extract_wikilinks;

/// Frontmatter identity key. Stamped by ingest; present on seeded notes.
pub const TOPODB_ID_KEY: &str = "topodb-id";
/// Seed's entity-link list. Becomes edges only — never a prop (fixpoint).
pub const RELATED_KEY: &str = "related";
/// Marks a seeded entity stub. Ingest skips these.
pub const ENTITY_STUB_KEY: &str = "entity";
/// Optional explicit title prop; seed uses it for filenames.
pub const TITLE_PROP: &str = "title";
