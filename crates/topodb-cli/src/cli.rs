//! `clap`-derive CLI surface: global args + the subcommand enum. [`Command`]
//! holds all 17 subcommands, covering info, writes, reads, maintenance, and
//! batch submission.

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
    /// `--props` merges additional structured metadata. `--scope` stamps
    /// this node's own scope (default: the global `--scope`).
    CreateMemory {
        #[arg(long)]
        content: String,
        /// Additional metadata as a JSON object string, e.g. '{"source":"chat"}'.
        #[arg(long)]
        props: Option<String>,
        /// Scope override for THIS command: a ScopeId ULID, or "shared".
        /// Defaults to the global `--scope`. Only the commands that STAMP a
        /// scope take this — a write lands in exactly one scope.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Create an entity node (person, project, concept). `name` is
    /// equality-indexed by the default spec (prop `name`, label `Entity` —
    /// see `topodb_json::ENTITY_*`); `--props` merges additional metadata.
    /// `--scope` stamps this node's own scope (default: the global `--scope`).
    CreateEntity {
        #[arg(long)]
        name: String,
        /// Additional metadata as a JSON object string.
        #[arg(long)]
        props: Option<String>,
        /// Scope override for THIS command: a ScopeId ULID, or "shared".
        /// Defaults to the global `--scope`. Only the commands that STAMP a
        /// scope take this — a write lands in exactly one scope.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Create a typed, time-aware edge between two existing nodes. `--scope`
    /// stamps the EDGE's own scope (default: the global `--scope`) — a `shared`
    /// edge is what lets two `shared` nodes stay connected across projects.
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
        /// Scope override for THIS command: a ScopeId ULID, or "shared".
        /// Defaults to the global `--scope`. Only the commands that STAMP a
        /// scope take this — a write lands in exactly one scope.
        #[arg(long)]
        scope: Option<String>,
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
    ///
    /// Deliberately **not** gated behind a flag, unlike `topodb-mcp`'s
    /// `get_changes` (which requires `--allow-unscoped-changes`). That gate
    /// stops an LLM from tripping over an *advertised* tool and replaying every
    /// other project's writes into its context — it is accident-prevention, not
    /// a security boundary. This CLI advertises nothing to a model, and whoever
    /// can run it already holds the db file.
    ///
    /// Accepted risk: an agent with shell access bypasses the MCP gate by
    /// calling this command against the same file. If a host ever drives this
    /// CLI from an agent loop, revisit that.
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
    /// Close an open edge, stamping its `valid_to`. `--valid-to` defaults to
    /// "now" (applier-resolved) when omitted. Rejected (exit 2) if the edge
    /// doesn't exist.
    CloseEdge {
        /// Edge id (ULID).
        id: String,
        /// Unix ms the edge becomes valid until; defaults to "now".
        #[arg(long = "valid-to")]
        valid_to: Option<i64>,
    },
    /// Attach a raw embedding vector to an existing node under `model`. The
    /// host computes the vector; TopoDB stores it as-is. Rejected (exit 2) if
    /// the node doesn't exist or the vector's dim conflicts with the model's
    /// existing vectors in scope.
    SetEmbedding {
        /// Node id (ULID).
        id: String,
        /// Embedding model name (namespaces the vector).
        #[arg(long)]
        model: String,
        /// Embedding as a JSON array of floats, e.g. '[0.1,0.2,0.3]'.
        #[arg(long)]
        vector: String,
    },
    /// Cosine vector search under one `model`, scoped to the default scope.
    /// The query is a raw float array (host-computed). Rejected (exit 2) if
    /// `--k` is 0 or the vector is empty.
    SearchVector {
        /// Embedding model name to search within.
        #[arg(long)]
        model: String,
        /// Query embedding as a JSON array of floats.
        #[arg(long)]
        vector: String,
        /// Max hits to return.
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Restrict scoring to these node ids; repeatable. Omit to score the
        /// whole scope.
        #[arg(long = "candidate")]
        candidate: Vec<String>,
    },
    /// Submit a batch of high-level commands (a JSON array) atomically. Each
    /// command's `op` matches an MCP tool name, but field names are the batch
    /// DSL's own (not always identical to the tool's param names); `#N` in an
    /// id field refers to the id produced by the Nth (earlier) command. Reads
    /// from the given file, or from stdin when the path is `-` or omitted.
    /// Prints `{"ids":[...]}` (null for commands that produce no id).
    /// All-or-nothing.
    ///
    /// Per-op fields: create_memory { content, scope?, props? };
    /// create_entity { name, scope?, props? };
    /// link { from, to, type, scope?, props?, valid_from? } — note link uses
    /// from/to/type, NOT the link tool's from_id/to_id/edge_type;
    /// set_node_props { id, props } (props value null removes that key);
    /// remove_node { id }; close_edge { id, valid_to? };
    /// set_embedding { id, model, vector }.
    Submit {
        /// Path to a JSON command array, or `-`/omitted for stdin.
        #[arg(default_value = "-")]
        input: String,
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
