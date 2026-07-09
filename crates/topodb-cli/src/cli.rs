//! `clap`-derive CLI surface: global args + the subcommand enum. Tasks 4-5
//! add write/read subcommands to [`Command`]; only `Info` exists so far.

use std::path::PathBuf;

#[derive(clap::Parser)]
#[command(
    name = "topodb",
    about = "Direct-embedded CLI over a TopoDB database file"
)]
pub struct Cli {
    /// Database file (or TOPODB_DB env).
    #[arg(long, env = "TOPODB_DB")]
    pub db: PathBuf,
    /// Default scope: a ScopeId ULID, or "shared".
    #[arg(long, default_value = "shared")]
    pub scope: String,
    /// Pretty-print JSON output.
    #[arg(long)]
    pub pretty: bool,
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Report the open database's path, format version, current op-log
    /// sequence number, index spec, and default scope.
    Info,
    /// Store a new memory node. `content` becomes the full-text-searchable
    /// body (prop `content`, label `Memory` — see `topodb_json::MEMORY_*`);
    /// `--props` merges additional structured metadata.
    CreateMemory {
        #[arg(long)]
        content: String,
        /// Additional metadata as a JSON object string, e.g. '{"source":"chat"}'.
        #[arg(long)]
        props: Option<String>,
    },
    /// Create an entity node (person, project, concept). `name` is
    /// equality-indexed by the default spec (prop `name`, label `Entity` —
    /// see `topodb_json::ENTITY_*`); `--props` merges additional metadata.
    CreateEntity {
        #[arg(long)]
        name: String,
        /// Additional metadata as a JSON object string.
        #[arg(long)]
        props: Option<String>,
    },
    /// Create a typed, time-aware edge between two existing nodes.
    Link {
        /// Source node id (ULID).
        #[arg(long)]
        from: String,
        /// Target node id (ULID).
        #[arg(long)]
        to: String,
        /// Free-form edge type.
        #[arg(long = "type")]
        ty: String,
        /// Additional edge metadata as a JSON object string.
        #[arg(long)]
        props: Option<String>,
        /// Unix ms the edge becomes valid from; defaults to "now" (applier-resolved).
        #[arg(long = "valid-from")]
        valid_from: Option<i64>,
    },
    // Task 5 adds the rest (get, find, search, traverse, access-stats,
    // changes, compact).
}
