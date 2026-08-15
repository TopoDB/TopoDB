//! Open Knowledge Format (OKF v0.2) bundle ⇄ TopoDB transforms. Deterministic,
//! no LLM. Mirrors `topodb-obsidian`'s shape (`note` · `mapping` · `links` ·
//! `ingest` · `seed` · `index` · `log` · `report` · `vault`), but for OKF's
//! nested-YAML frontmatter, `[label](/path.md)` markdown links, and reserved
//! `index.md`/`log.md` files.
//!
//! A concept page = an Entity (carrying a bundle-relative `path` prop, the
//! identity anchor) + one attached `about` Memory. High-value provenance
//! (`generated`/`verified`/`sources`) is promoted to actor/source entities and
//! edges; the long tail is flattened to dotted-key props. Every promotion is
//! reversible so `seed → ingest → seed` and `ingest → seed → ingest` are
//! no-ops.

mod index;
mod ingest;
mod links;
mod log;
mod mapping;
mod note;
mod report;
mod seed;
mod vault;

pub use ingest::{ingest_okf, EmbedFn};
pub use links::{extract_links, resolve_link};
pub use note::Note;
pub use report::{FileError, IngestReport, SeedReport};
pub use seed::seed_okf;
pub use vault::walk_bundle;

use topodb::IndexSpec;

/// Frontmatter identity key (our OKF extension stamp). Absent on a foreign
/// bundle's first ingest; stamped thereafter and on every seeded page.
pub const TOPODB_ID_KEY: &str = "topodb-id";

/// Bundle-relative path prop — the OKF identity anchor. Equality-indexed by
/// [`okf_spec`] so body links (`find_by_prop`) and re-ingest path dedup resolve.
pub const PATH_PROP: &str = "path";

/// The OKF bundle marker, carried on the root `index.md`.
pub const OKF_VERSION: &str = "0.2";
pub const OKF_VERSION_KEY: &str = "okf_version";

/// Entity `kind` marker for promoted provenance nodes (concepts have `type`,
/// never `kind`).
pub const ENTITY_KIND_PROP: &str = "kind";
pub const KIND_ACTOR: &str = "actor";
pub const KIND_SOURCE: &str = "source";

/// The OKF `type` of a concept, stored as an entity prop (open vocabulary,
/// never validated).
pub const TYPE_PROP: &str = "type";
pub const TITLE_KEY: &str = "title";
pub const TAGS_KEY: &str = "tags";

// Promoted-frontmatter YAML keys.
pub const GENERATED_KEY: &str = "generated";
pub const VERIFIED_KEY: &str = "verified";
pub const SOURCES_KEY: &str = "sources";
pub const BY_KEY: &str = "by";
pub const AT_KEY: &str = "at";
pub const RESOURCE_KEY: &str = "resource";
pub const AUTHOR_KEY: &str = "author";

// Edge types.
pub const ABOUT_EDGE: &str = "about";
pub const REFERENCES_EDGE: &str = "references";
pub const GENERATED_BY_EDGE: &str = "generated_by";
pub const VERIFIED_BY_EDGE: &str = "verified_by";
pub const SOURCED_FROM_EDGE: &str = "sourced_from";
pub const AUTHORED_BY_EDGE: &str = "authored_by";

/// Edge prop carrying a provenance timestamp (`generated`/`verified` `at`).
pub const AT_PROP: &str = "at";

/// Bundle files skipped by the walk: reserved and scratch names (design
/// §Ingest step 1). `index.md` is handled separately (never a concept).
pub const SKIP_FILES: [&str; 4] = ["log.md", "_plan.md", "_skeleton.md", "INSTRUCTIONS.md"];

/// The index spec ingest/seed require: the JSON default plus an equality index
/// on `(Entity, path)` so path dedup and body-link resolution work.
pub fn okf_spec() -> IndexSpec {
    let mut spec = topodb_json::default_spec();
    spec.equality.push(topodb::PropIndex {
        label: topodb_json::ENTITY_LABEL.into(),
        prop: PATH_PROP.into(),
    });
    spec
}
