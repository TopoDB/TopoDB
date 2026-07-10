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
    /// Fetch one node by id. `{"found":false}` (exit 0) if it doesn't exist
    /// or is out of the default scope — the two are indistinguishable by
    /// design.
    Get {
        /// Node id (ULID).
        id: String,
    },
    /// Exact-match lookup on an equality-indexed `(label, prop)` pair.
    /// Errors (exit 2) if that pair isn't declared in the open db's index
    /// spec, or `--value` is a float (floats aren't equality-indexable).
    Find {
        #[arg(long)]
        label: String,
        #[arg(long)]
        prop: String,
        /// Parsed as a JSON scalar (e.g. `42`, `true`, `"ada"`); a value that
        /// doesn't parse as JSON is taken as a bare string (so `--value ada`
        /// and `--value '"ada"'` are equivalent).
        #[arg(long)]
        value: String,
    },
    /// Full-text BM25 search over indexed text properties.
    Search {
        /// The search query.
        query: String,
        /// Max hits to return.
        #[arg(long, default_value_t = 10)]
        k: usize,
    },
    /// Bounded BFS from a seed node, following edges up to `max_hops`.
    Traverse {
        /// Seed node id (ULID) to start from.
        seed: String,
        /// Hop budget (1-4).
        #[arg(long = "max-hops", default_value_t = 2)]
        max_hops: u8,
        /// Which adjacency to follow from each frontier node.
        #[arg(long, value_enum, default_value_t = DirectionArg::Both)]
        direction: DirectionArg,
        /// Restrict the walk to these edge types; repeatable. Omit to follow
        /// every type.
        #[arg(long = "edge-type")]
        edge_type: Vec<String>,
    },
    /// Read a node's access statistics (count, last-accessed timestamp).
    /// `{"found":false}` (exit 0) if the node doesn't exist or is out of the
    /// default scope. Reading stats never itself counts as an access.
    Stats {
        /// Node id (ULID).
        id: String,
    },
    /// Replay the op log from a sequence number (inclusive). Unscoped
    /// host-level primitive — spans every scope. `Compacted` (the requested
    /// range is below the retained floor) is a rejected/exit-2 condition:
    /// the caller re-anchors from current state rather than trusting a
    /// truncated tail.
    Changes {
        /// Op-log sequence number to replay from, inclusive.
        #[arg(long)]
        since: u64,
    },
    /// Compact the durable op log, dropping every entry with seq <
    /// `keep_from`.
    Compact {
        #[arg(long = "keep-from")]
        keep_from: u64,
    },
    /// Set or remove properties on an existing node. `--props` is a JSON
    /// object; a `null` value REMOVES that key, any other scalar sets it.
    /// Rejected (exit 2) if the node doesn't exist.
    SetProps {
        /// Node id (ULID).
        id: String,
        /// Property changes as a JSON object, e.g. '{"role":"x","stale":null}'.
        #[arg(long)]
        props: String,
    },
    /// Hard-delete a node and cascade-remove its incident edges. Rejected
    /// (exit 2) if the node doesn't exist.
    RemoveNode {
        /// Node id (ULID).
        id: String,
    },
}

/// Wire form of `topodb::Direction` for `--direction`: lowercase
/// `out`/`in`/`both`, matching the MCP server's `DirectionParam` vocabulary.
#[derive(clap::ValueEnum, Debug, Clone, Copy, Default)]
pub enum DirectionArg {
    Out,
    In,
    #[default]
    Both,
}

impl From<DirectionArg> for topodb::Direction {
    fn from(d: DirectionArg) -> Self {
        match d {
            DirectionArg::Out => topodb::Direction::Out,
            DirectionArg::In => topodb::Direction::In,
            DirectionArg::Both => topodb::Direction::Both,
        }
    }
}
