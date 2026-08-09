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
    /// Database file. Resolution order: this flag, then TOPODB_DB, then a
    /// `.topodb.toml` on the path from cwd up, then ~/.topodb/memory.redb.
    #[arg(long)]
    pub db: Option<PathBuf>,
    /// Default scope: a ScopeId ULID, or "shared". Resolution order: this
    /// flag, then TOPODB_SCOPE, then `.topodb.toml`, then "shared".
    #[arg(long)]
    pub scope: Option<String>,
    /// Pretty-print JSON output.
    #[arg(long, global = true)]
    pub pretty: bool,
    /// Milliseconds to wait (retrying with backoff) when another process
    /// holds the database file, before failing with kind "busy" / exit 3.
    /// 0 = fail immediately.
    #[arg(
        long,
        global = true,
        env = "TOPODB_LOCK_WAIT_MS",
        default_value_t = 3000
    )]
    pub lock_wait_ms: u64,
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
        /// Opt OUT of find-or-create: always mint a new node, even when an
        /// entity with this name already exists in the write scope (the
        /// pre-0.0.13 behavior). Duplicate names fragment traversal — only
        /// use this when two same-named entities are genuinely distinct.
        #[arg(long)]
        always_create: bool,
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
    /// Store a fact and link it to its entities in ONE atomic write:
    /// exact-content dedup (whitespace-normalized), find-or-create for each
    /// `--entity` name (alias-aware, oldest node wins), one edge per entity
    /// (`--edge-type`, default "about", vocabulary-normalized), and optional
    /// retirement of replaced memories via `--supersedes`. The composed,
    /// fragmentation-proof alternative to create-memory + create-entity +
    /// link. Prints memory_id, per-entity {name,id,created}, edge_ids,
    /// deduplicated, superseded.
    Remember {
        /// The fact to store (full-text-searchable body). Pass it as a
        /// positional argument, or via --content (not both).
        content: Option<String>,
        /// Alias for the positional fact. Conflicts with the positional form.
        #[arg(long = "content", value_name = "CONTENT", conflicts_with = "content")]
        content_flag: Option<String>,
        /// Entity name to link the memory to; repeatable, at least one.
        #[arg(long = "entity", required = true)]
        entity: Vec<String>,
        /// Edge type for the memory->entity links (default: "about").
        #[arg(long)]
        edge_type: Option<String>,
        /// Memory id this fact replaces; repeatable. Marks it superseded
        /// (recall drops it from "now" onward; history keeps it).
        #[arg(long = "supersedes")]
        supersedes: Vec<String>,
        /// Additional metadata as a JSON object string.
        #[arg(long)]
        props: Option<String>,
        /// Scope override for THIS command: a ScopeId ULID, or "shared".
        #[arg(long)]
        scope: Option<String>,
        /// Taxonomy kind for a NEW memory: "episodic" (dated observation),
        /// "semantic" (standing fact — the default reading when omitted),
        /// or "procedural" (how-to). Ignored on a dedup hit: the existing
        /// memory's stored kind wins.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Soft-retire memories: stamps `forgotten_at` and closes their open
    /// edges. Recall and default `search` stop returning them; history
    /// stays reachable via `search --include-superseded` and temporal reads.
    /// Every id must be a live Memory in the write scope — any invalid id
    /// rejects the whole call.
    Forget {
        /// Memory ids (ULIDs) to forget.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Write scope for this command: a scope ULID or "shared".
        /// Overrides the global --scope.
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
        /// Match string values case- and whitespace-insensitively
        /// (`"drew powell"` finds `"Drew Powell"`) instead of byte-exactly.
        #[arg(long)]
        normalized: bool,
    },
    /// Full-text BM25 search over indexed text properties. Memories retired
    /// (superseded or forgotten) are skipped by default; pass
    /// `--include-superseded` to search history too.
    Search {
        /// The search query.
        query: String,
        /// Max hits to return.
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Also return retired memories (superseded OR forgotten) — the
        /// history escape hatch, same shape as `get-edges --open-only false`.
        #[arg(long)]
        include_superseded: bool,
        /// Only return memories of these kinds ("episodic" | "semantic" |
        /// "procedural"); repeatable or comma-delimited. A node without a
        /// kind prop counts as "semantic" — note that covers non-Memory
        /// nodes (entities) too, so a filter excluding "semantic" hides
        /// them. Omit for no kind filtering.
        #[arg(long = "kinds", value_delimiter = ',')]
        kinds: Vec<String>,
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
        /// View the graph as it was at this Unix-millisecond instant: only
        /// edges live at that time are followed (closed edges whose
        /// validity covered it reappear; later edges vanish). A future
        /// as_of behaves like "now". Omitted = now.
        #[arg(long)]
        as_of: Option<i64>,
    },
    /// List edges from a source node, with optional time-travel (as_of) and
    /// history access (open_only). Defaults to currently-open edges when neither
    /// flag is set; pass `--open-only false` to see closed edges (history).
    /// `--as-of <UNIX_MS>` filters to edges live at that Unix-millisecond instant
    /// (mutually exclusive with `--open-only`).
    GetEdges {
        /// Source node id (ULID) whose outgoing edges to list.
        from: String,
        /// Restrict to edges pointing at this target node ULID.
        #[arg(long)]
        to: Option<String>,
        /// Restrict to this edge type (normalized like `link` normalizes it;
        /// edges stored under the raw un-normalized form are matched too).
        #[arg(long = "edge-type")]
        edge_type: Option<String>,
        /// Only edges currently open (default true); pass false to include closed
        /// edges (full history). Mutually exclusive with `--as-of` — omit this flag
        /// when passing `--as-of`.
        #[arg(long, value_name = "true|false")]
        open_only: Option<bool>,
        /// View edges as they were at this Unix-millisecond instant: only edges
        /// live at that time. Mutually exclusive with `--open-only` — omit
        /// `--open-only` when passing `--as-of`. Must be a positive timestamp.
        #[arg(long)]
        as_of: Option<i64>,
        /// Which direction to follow: `out` (from node → target, default),
        /// `in` (target ← from node), or `both` (union, id-deduped).
        /// For `in`, the positional node is the target and `--to` filters sources;
        /// `--to` filters the far end of each edge, whichever side that is.
        #[arg(long, value_enum, default_value_t = DirectionArg::Out)]
        direction: DirectionArg,
    },
    /// Read a node's access statistics (count, last-accessed timestamp).
    /// `{"found":false}` (exit 0) if the node doesn't exist or is out of the
    /// default scope. Reading stats never itself counts as an access.
    Stats {
        /// Node id (ULID).
        id: String,
    },
    /// Surface decay candidates: live memories ranked by kind-aware
    /// staleness ((age/half_life)/ln(e+access_count); age since last
    /// access, falling back to creation). Read-only and unbumped — the
    /// sweep PROPOSES, it never stamps; act on its output with `forget`.
    /// Half-life defaults: episodic 14d, semantic 120d, procedural 365d
    /// (absent/unknown kind counts as semantic).
    LifecycleCandidates {
        /// Top-N candidates to report.
        #[arg(long, default_value_t = topodb_json::LIFECYCLE_DEFAULT_LIMIT)]
        limit: usize,
        #[arg(long = "half-life-episodic-days", default_value_t = topodb_json::LIFECYCLE_HALF_LIFE_EPISODIC_DAYS)]
        half_life_episodic_days: f64,
        #[arg(long = "half-life-semantic-days", default_value_t = topodb_json::LIFECYCLE_HALF_LIFE_SEMANTIC_DAYS)]
        half_life_semantic_days: f64,
        #[arg(long = "half-life-procedural-days", default_value_t = topodb_json::LIFECYCLE_HALF_LIFE_PROCEDURAL_DAYS)]
        half_life_procedural_days: f64,
        /// Pin the sweep's "now" (Unix ms) for reproducible runs; omitted =
        /// wall clock.
        #[arg(long = "now-ms")]
        now_ms: Option<i64>,
    },
    /// DESTRUCTIVE space reclamation: hard-delete every Memory whose
    /// superseded_at or forgotten_at tombstone is strictly older than the
    /// cutoff (engine remove-node; incident edges cascade away). Dry-run
    /// by default — prints count + ids and writes NOTHING until --yes.
    /// Purged history is gone: as_of queries stop seeing those nodes.
    /// Deliberately CLI-only and never part of the sgh lifecycle graph.
    Purge {
        /// Unix-ms cutoff: purge tombstones strictly older than this.
        #[arg(long = "tombstoned-before")]
        tombstoned_before: i64,
        /// Actually delete. Without it, purge is a dry-run report.
        #[arg(long)]
        yes: bool,
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
    /// id field refers to the id produced by the Nth earlier command (0-indexed:
    /// `#0` is the first command). Reads from the given file, or from stdin when
    /// the path is `-` or omitted. Prints `{"ids":[...]}` (null for commands
    /// that produce no id). All-or-nothing.
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
    /// Ingest an Obsidian-format vault: one note = one memory; wikilinks
    /// become entities; edited notes supersede their prior version.
    ObsidianIngest {
        /// Vault directory to walk for .md notes.
        vault: std::path::PathBuf,
        #[arg(long)]
        scope: Option<String>,
        /// Plan and report without writing to the db or the vault.
        #[arg(long)]
        dry_run: bool,
    },
    /// Materialize memories from the graph into an Obsidian-format vault
    /// (one note per memory + entity stubs). Never overwrites differing
    /// files unless --overwrite.
    ObsidianSeed {
        /// Vault directory to write notes into (created if missing).
        vault: std::path::PathBuf,
        /// Hybrid-recall selector (exclusive with --entity).
        #[arg(long)]
        query: Option<String>,
        #[arg(long, default_value_t = 12)]
        k: usize,
        /// Entity-neighborhood selector (exclusive with --query).
        #[arg(long)]
        entity: Option<String>,
        #[arg(long, default_value_t = 2)]
        hops: u8,
        #[arg(long)]
        overwrite: bool,
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
