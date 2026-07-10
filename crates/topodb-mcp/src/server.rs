//! The rmcp server handler wrapping a TopoDB [`Db`].
//!
//! Built on rmcp 2.2.0: the tool surface is declared with `#[tool_router]` +
//! `#[tool]` and dispatched through `#[tool_handler]` on the [`ServerHandler`]
//! impl. Task 4 added six read tools (`get_node`, `find_by_prop`,
//! `search_memories`, `traverse`, `access_stats`, `get_changes`), following
//! the `db_info` pattern established in Task 3. Task 5 adds three write tools
//! (`create_memory`, `create_entity`, `link`) — each one `Db::submit` call
//! (atomic). Every tool resolves its optional `scope` param via
//! [`TopoServer::resolve_scopes`] (reads) or [`TopoServer::resolve_scope`]
//! (writes) and maps engine `Err`s to `ErrorData` through `topodb_json`
//! (imported here as `convert`) — never panics.

use std::str::FromStr;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use topodb::{
    Db, Direction, EdgeId, NodeId, Op, PropValue, Props, Scope, ScopeSet, TopoError,
    TraversalQuery, VectorQuery,
};

use crate::config::{
    scope_label, Config, ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP, MEMORY_LABEL,
};
use topodb_json as convert;

/// The MCP server state. `Clone` is required by rmcp (the service clones the
/// handler per request); every field is cheap to clone — [`Db`] is an `Arc`
/// handle, [`ScopeSet`] is a small set, and the rest are owned metadata.
#[derive(Clone)]
pub struct TopoServer {
    db: Db,
    /// The configured default scope, applied to tool calls that omit `scope`.
    default_scope: Scope,
    /// `default_scope` pre-resolved to a [`ScopeSet`], reused by every scoped
    /// read tool call that omits `scope` (see [`TopoServer::resolve_scopes`]).
    default_scopes: ScopeSet,
    /// Rendered db path, reported by `db_info`.
    db_path: String,
    tool_router: ToolRouter<Self>,
}

impl TopoServer {
    /// Wraps an open [`Db`] and the resolved [`Config`] into a server handler.
    pub fn new(db: Db, config: &Config) -> Self {
        let default_scopes = convert::scope_to_scope_set(config.default_scope);
        Self {
            db,
            default_scope: config.default_scope,
            default_scopes,
            db_path: config.db_path.display().to_string(),
            tool_router: Self::tool_router(),
        }
    }

    /// Resolves a tool call's optional `scope` param to the [`ScopeSet`] the
    /// read should run against. `None` reuses the pre-resolved
    /// `default_scopes` (no need to re-derive it on every call that omits
    /// `scope` — the common case); `Some(s)` is parsed fresh via
    /// [`convert::resolve_scope`].
    fn resolve_scopes(&self, scope: Option<&str>) -> Result<ScopeSet, ErrorData> {
        match scope {
            None => Ok(self.default_scopes.clone()),
            Some(_) => {
                let resolved = convert::resolve_scope(scope, self.default_scope)
                    .map_err(|e| ErrorData::invalid_params(e, None))?;
                Ok(convert::scope_to_scope_set(resolved))
            }
        }
    }

    /// Resolves a write tool's optional `scope` param to the single [`Scope`]
    /// the created node/edge is stamped with. Unlike `resolve_scopes` (which
    /// expands to a `ScopeSet` for reads), a write needs exactly one `Scope`
    /// value, not a set to filter by — so this goes through
    /// [`convert::resolve_scope`] directly rather than also converting to a
    /// `ScopeSet`. `link` has no `scope` param on the wire (per the plan's
    /// tool table) and always calls this with `None`, which still resolves
    /// through the same path to the server's configured default scope.
    fn resolve_scope(&self, scope: Option<&str>) -> Result<Scope, ErrorData> {
        convert::resolve_scope(scope, self.default_scope)
            .map_err(|e| ErrorData::invalid_params(e, None))
    }

    /// Submits a one-op write batch (every Task 5 write tool is exactly one
    /// `CreateNode`/`CreateEdge`, so the batch is trivially atomic).
    /// `TopoError::Rejected` (e.g. `link`'s missing-endpoint check) is a
    /// caller-fixable input problem → `invalid_params`; every other error
    /// (storage, encoding, a closed engine) → `internal_error` — the same
    /// classification `search_memories`/`get_changes` already use (Task 4's
    /// review-fix pattern).
    fn submit_write(&self, ops: Vec<Op>) -> Result<(), ErrorData> {
        self.db.submit(ops).map(|_| ()).map_err(classify_topo_error)
    }

    /// Like [`submit_write`], but returns the batch's `last_seq` for tools that
    /// report the committed sequence number (set_node_props, remove_node,
    /// close_edge, set_embedding). Same error classification as `submit_write`.
    fn submit_seq(&self, ops: Vec<Op>) -> Result<u64, ErrorData> {
        self.db
            .submit(ops)
            .map(|a| a.last_seq)
            .map_err(classify_topo_error)
    }
}

/// Maps an engine `TopoError` to the right `ErrorData`: `Rejected` (caller
/// -fixable bad input) → `invalid_params`; every other variant → `internal_error`.
/// Shared by the `submit_*` write helpers and the read tools that classify
/// engine errors this way.
fn classify_topo_error(e: TopoError) -> ErrorData {
    match e {
        TopoError::Rejected(msg) => ErrorData::invalid_params(msg, None),
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

/// Schema stand-in for a props map. The tool bodies keep taking a raw
/// [`Value`] (so `convert::json_to_props` owns validation and its error
/// messages), but the *advertised* schema must say "object" — see
/// [`prop_value_schema`] and `tests/schema.rs` for why a typeless param is a
/// wire-level bug.
type PropsSchema = std::collections::BTreeMap<String, Value>;

/// Schema stand-in for a raw embedding: an array of numbers.
type VectorSchema = Vec<f64>;

/// Schema stand-in for `submit_batch`'s command list: an array of objects.
type CommandsSchema = Vec<Value>;

/// The JSON Schema for `find_by_prop`'s `value`: the equality-indexable
/// scalars. Floats are excluded deliberately — `IndexValue::of` rejects them.
///
/// Spelled out by hand because `serde_json::Value` renders as a *typeless*
/// (permissive) schema. A client reading `{"description": "..."}` has nothing
/// to encode against and may send `"1815"` where `1815` was meant — and since
/// a string is itself a legal `value`, that mismatch would silently return
/// zero rows rather than erroring. See `tests/schema.rs`.
fn prop_value_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["string", "integer", "boolean"],
    })
}

/// Parses a tool-supplied ULID string into a [`NodeId`], mapping a parse
/// failure to `invalid_params` (never a panic).
fn parse_node_id(id: &str) -> Result<NodeId, ErrorData> {
    NodeId::from_str(id)
        .map_err(|e| ErrorData::invalid_params(format!("invalid node id {id:?}: {e}"), None))
}

/// The `db_info` result payload. `Json<DbInfo>` (below) makes it structured
/// tool output.
#[derive(Debug, Serialize, JsonSchema)]
struct DbInfo {
    /// Filesystem path of the open database.
    path: String,
    /// Highest op-log sequence number committed so far (0 on a fresh db). Use
    /// this as the `since_seq` anchor for `get_changes`.
    current_seq: u64,
    /// Default scope applied to tool calls that omit `scope`: `"shared"` or a
    /// ULID string.
    default_scope: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetNodeParams {
    /// ULID of the node to fetch.
    id: String,
    /// Scope to look the node up in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetNodeResult {
    /// Whether the node exists and is visible in the resolved scope. `false`
    /// covers both "no such node" and "exists but out of scope" — the two
    /// are indistinguishable by design (see `Db::node`).
    found: bool,
    /// Present only when `found` is `true`: the node's id/scope/label/props.
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindByPropParams {
    /// Node label to match, e.g. `"Entity"`.
    label: String,
    /// Property name to match — must be declared in the index spec's
    /// equality list for this label.
    prop: String,
    /// Value to match exactly: a string, integer, or boolean (floats are not
    /// equality-indexable).
    #[schemars(schema_with = "prop_value_schema")]
    value: Value,
    /// Scope to search in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FindByPropResult {
    /// Every matching node (id/scope/label/props), in `Db::nodes_by_prop`'s
    /// unspecified but deterministic-per-call order.
    nodes: Vec<Value>,
}

fn default_search_k() -> usize {
    10
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchMemoriesParams {
    /// Free-text query.
    query: String,
    /// Maximum number of results to return. Must be at least 1 — `search_text`
    /// rejects `k == 0`.
    #[serde(default = "default_search_k")]
    #[schemars(range(min = 1))]
    k: usize,
    /// Scope to search in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchHit {
    /// The matched node (id/scope/label/props).
    node: Value,
    /// Relevance score — BM25 for text search, cosine similarity for vector search (higher is more relevant).
    score: f32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchMemoriesResult {
    /// Up to `k` hits, ranked by descending relevance.
    hits: Vec<SearchHit>,
}

/// Wire form of `topodb::Direction` for the `traverse` tool's `direction`
/// param: lowercase to match the plan's `out`/`in`/`both` vocabulary.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DirectionParam {
    Out,
    In,
    #[default]
    Both,
}

impl From<DirectionParam> for Direction {
    fn from(d: DirectionParam) -> Self {
        match d {
            DirectionParam::Out => Direction::Out,
            DirectionParam::In => Direction::In,
            DirectionParam::Both => Direction::Both,
        }
    }
}

fn default_max_hops() -> u8 {
    2
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TraverseParams {
    /// ULID of the node to start the traversal from.
    seed_id: String,
    /// Hop budget (1-4). Out-of-range values are rejected, not clamped — the
    /// bound is advertised so a client never sends one.
    #[serde(default = "default_max_hops")]
    #[schemars(range(min = 1, max = 4))]
    max_hops: u8,
    /// Which adjacency to follow from each frontier node: `"out"`, `"in"`, or
    /// `"both"`.
    #[serde(default)]
    direction: DirectionParam,
    /// Restrict the walk to these edge types; omit to follow every type.
    #[serde(default)]
    edge_types: Option<Vec<String>>,
    /// Scope to traverse in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TraverseResult {
    /// `{"nodes": [...], "edges": [...]}` reached from the seed.
    subgraph: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AccessStatsParams {
    /// ULID of the node.
    id: String,
    /// Scope to look the node up in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AccessStatsResult {
    /// Whether the node exists and is visible in the resolved scope (same
    /// found/not-found semantics as `get_node`).
    found: bool,
    /// Present only when `found` is `true`: how many times the node has been
    /// returned by a scoped read.
    #[serde(skip_serializing_if = "Option::is_none")]
    access_count: Option<u64>,
    /// Present only when `found` is `true`: wall-clock ms timestamp of the
    /// most recent such read (0 if the node has never been counted).
    #[serde(skip_serializing_if = "Option::is_none")]
    last_accessed_at: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetChangesParams {
    /// Op-log sequence number to replay from, inclusive.
    since_seq: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ChangeEventJson {
    /// The op's position in the durable op log.
    seq: u64,
    /// The committed op itself.
    op: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetChangesResult {
    /// Ops in ascending `seq` order, starting at `since_seq`.
    ops: Vec<ChangeEventJson>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateMemoryParams {
    /// The memory's full-text-searchable body.
    content: String,
    /// Structured metadata merged into the node's props (string/number/bool
    /// values). Must not include a `content` key — that key is set from the
    /// `content` param above; a collision is rejected rather than silently
    /// overwritten.
    #[serde(default)]
    #[schemars(with = "Option<PropsSchema>")]
    props: Option<Value>,
    /// Scope to create the memory in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateEntityParams {
    /// The entity's equality-indexed identifying name.
    name: String,
    /// Structured metadata merged into the node's props (string/number/bool
    /// values). Must not include a `name` key — that key is set from the
    /// `name` param above; a collision is rejected rather than silently
    /// overwritten.
    #[serde(default)]
    #[schemars(with = "Option<PropsSchema>")]
    props: Option<Value>,
    /// Scope to create the entity in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CreateResult {
    /// ULID of the newly created node.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LinkParams {
    /// ULID of the edge's source (`from`) node. Must already exist.
    from_id: String,
    /// ULID of the edge's target (`to`) node. Must already exist.
    to_id: String,
    /// Free-form edge type (e.g. `"works_on"`, `"about"`). Be consistent —
    /// `traverse` can filter by it.
    edge_type: String,
    /// Structured metadata on the edge (string/number/bool values).
    #[serde(default)]
    #[schemars(with = "Option<PropsSchema>")]
    props: Option<Value>,
    /// Milliseconds since Unix epoch the edge becomes valid from. Defaults to
    /// "now" (resolved by the engine at commit time) when omitted.
    #[serde(default)]
    valid_from: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LinkResult {
    /// ULID of the newly created edge.
    id: String,
}

/// The `{ "seq": <last_seq> }` result shared by the mutating tools that don't
/// create a node/edge (set_node_props, remove_node, close_edge, set_embedding).
#[derive(Debug, Serialize, JsonSchema)]
struct SeqResult {
    /// The committed op-log sequence number of this write (anchor for
    /// get_changes).
    seq: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetNodePropsParams {
    /// ULID of the node to update.
    id: String,
    /// Property changes: a `null` value REMOVES the key, any other scalar sets
    /// it.
    #[schemars(with = "PropsSchema")]
    props: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveNodeParams {
    /// ULID of the node to hard-delete (its incident edges cascade away).
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CloseEdgeParams {
    /// ULID of the edge to close.
    id: String,
    /// Unix ms the edge becomes valid until; defaults to "now" (engine
    /// -resolved) when omitted.
    #[serde(default)]
    valid_to: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetEmbeddingParams {
    /// ULID of the node to attach the embedding to.
    id: String,
    /// Embedding model name (namespaces the vector).
    model: String,
    /// Raw embedding as a JSON array of finite numbers (host-computed).
    #[schemars(with = "VectorSchema")]
    vector: Value,
}

fn default_vector_k() -> usize {
    10
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchVectorsParams {
    /// Embedding model name to search within.
    model: String,
    /// Query embedding as a JSON array of finite numbers (host-computed).
    #[schemars(with = "VectorSchema")]
    vector: Value,
    /// Maximum number of results to return. Must be at least 1 —
    /// `search_vector` rejects `k == 0`.
    #[serde(default = "default_vector_k")]
    #[schemars(range(min = 1))]
    k: usize,
    /// Scope to search in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Restrict scoring to these node ULIDs (e.g. a traversal result). Omit to
    /// score the whole scope.
    #[serde(default)]
    candidates: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchVectorsResult {
    /// Up to `k` hits, ranked by descending cosine similarity.
    hits: Vec<SearchHit>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SubmitBatchParams {
    /// A JSON array of high-level commands. Each command's `op` matches an MCP
    /// tool name (create_memory, create_entity, link, set_node_props,
    /// remove_node, close_edge, set_embedding); `#N` in an id field refers to
    /// the id produced by the Nth (earlier) command in the batch.
    #[schemars(with = "CommandsSchema")]
    commands: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SubmitBatchResult {
    /// One entry per command, in order: the produced node/edge ULID, or null
    /// for commands that create nothing (set_node_props, remove_node,
    /// close_edge, set_embedding).
    ids: Vec<Option<String>>,
}

#[tool_router]
impl TopoServer {
    #[tool(
        description = "Report the open database's path, current op-log sequence number, and the default scope applied to tool calls that omit `scope`. Call this first to confirm the server is wired to the expected database, and to obtain current_seq as the anchor for get_changes."
    )]
    fn db_info(&self) -> Result<Json<DbInfo>, ErrorData> {
        let current_seq = self
            .db
            .current_seq()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(Json(DbInfo {
            path: self.db_path.clone(),
            current_seq,
            default_scope: scope_label(&self.default_scope),
        }))
    }

    #[tool(
        description = "Fetch one node by its ULID. Call this when you already have a node id (from a previous search, traverse, or create) and need its current label and properties."
    )]
    fn get_node(
        &self,
        Parameters(p): Parameters<GetNodeParams>,
    ) -> Result<Json<GetNodeResult>, ErrorData> {
        let id = parse_node_id(&p.id)?;
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
        match self.db.node(&scopes, id) {
            Some(n) => {
                let node =
                    convert::node_to_json(&n).map_err(|e| ErrorData::internal_error(e, None))?;
                Ok(Json(GetNodeResult {
                    found: true,
                    node: Some(node),
                }))
            }
            None => Ok(Json(GetNodeResult {
                found: false,
                node: None,
            })),
        }
    }

    #[tool(
        description = "Exact-match lookup on an equality-indexed property (e.g. an Entity's name). Call this to resolve a known identifier to a node — NOT for fuzzy or full-text search; use search_memories for that. Errors if (label, prop) is not declared in the index spec."
    )]
    fn find_by_prop(
        &self,
        Parameters(p): Parameters<FindByPropParams>,
    ) -> Result<Json<FindByPropResult>, ErrorData> {
        let value = convert::json_to_prop_value(&p.value)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
        // `nodes_by_prop` is a pure snapshot read: the only error it can
        // produce today is `Rejected` (undeclared index / Float value), so a
        // blanket `invalid_params` is accurate. Reconsider if the engine path
        // ever grows storage-touching failure modes (see `search_memories`).
        let hits = self
            .db
            .nodes_by_prop(&scopes, &p.label, &p.prop, &value)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let nodes = hits
            .iter()
            .map(convert::node_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(FindByPropResult { nodes }))
    }

    #[tool(
        description = "Full-text BM25 search over indexed text properties. Call this when looking for memories relevant to a topic or phrase. Returns up to k nodes ranked by relevance with scores."
    )]
    fn search_memories(
        &self,
        Parameters(p): Parameters<SearchMemoriesParams>,
    ) -> Result<Json<SearchMemoriesResult>, ErrorData> {
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
        // `search_text` opens a redb read transaction, so unlike the pure
        // snapshot reads it CAN fail with `Storage`/`Encoding` — only its
        // input-validation `Rejected` (k == 0, token-less query) maps to
        // invalid_params; everything else is a server-side internal_error.
        let hits = self
            .db
            .search_text(&scopes, &p.query, p.k)
            .map_err(|e| match e {
                TopoError::Rejected(_) => ErrorData::invalid_params(e.to_string(), None),
                other => ErrorData::internal_error(other.to_string(), None),
            })?;
        let hits = hits
            .iter()
            .map(|(n, score)| {
                convert::node_to_json(n).map(|node| SearchHit {
                    node,
                    score: *score,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(SearchMemoriesResult { hits }))
    }

    #[tool(
        description = "Walk the graph outward from a seed node, following edges up to max_hops. Call this to gather the context AROUND something you already found — related entities, linked memories. Returns the subgraph (nodes + edges)."
    )]
    fn traverse(
        &self,
        Parameters(p): Parameters<TraverseParams>,
    ) -> Result<Json<TraverseResult>, ErrorData> {
        let seed = parse_node_id(&p.seed_id)?;
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
        let query = TraversalQuery {
            scopes,
            seeds: vec![seed],
            max_hops: p.max_hops,
            edge_types: p
                .edge_types
                .map(|v| v.into_iter().map(Into::into).collect()),
            direction: p.direction.into(),
            as_of: None,
        };
        // `traverse` is a pure snapshot BFS: the only error it can produce
        // today is `Rejected` (max_hops out of 1..=4), so a blanket
        // `invalid_params` is accurate. Reconsider if the engine path ever
        // grows storage-touching failure modes (see `search_memories`).
        let sg = self
            .db
            .traverse(&query)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let subgraph =
            convert::subgraph_to_json(&sg).map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(TraverseResult { subgraph }))
    }

    #[tool(
        description = "Read a node's access statistics (count, last-accessed timestamp). Call this when deciding what to consolidate or forget — e.g. finding stale memories. Reading stats does not itself count as an access."
    )]
    fn access_stats(
        &self,
        Parameters(p): Parameters<AccessStatsParams>,
    ) -> Result<Json<AccessStatsResult>, ErrorData> {
        let id = parse_node_id(&p.id)?;
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
        let stats = self
            .db
            .access_stats(&scopes, id)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(Json(match stats {
            Some(s) => AccessStatsResult {
                found: true,
                access_count: Some(s.access_count),
                last_accessed_at: Some(s.last_accessed_at),
            },
            None => AccessStatsResult {
                found: false,
                access_count: None,
                last_accessed_at: None,
            },
        }))
    }

    #[tool(
        description = "Replay the operation log from a sequence number (inclusive). Host-level primitive for consolidation/sync — the ONE unscoped read; the log spans all scopes. Returns ops with their seq numbers; on Compacted errors, re-anchor from current state. The db_info tool reports current_seq."
    )]
    fn get_changes(
        &self,
        Parameters(p): Parameters<GetChangesParams>,
    ) -> Result<Json<GetChangesResult>, ErrorData> {
        let events = self.db.ops_since(p.since_seq).map_err(|e| match e {
            // Carries `oldest` in the message (TopoError::Compacted's Display
            // already renders it) so the caller can re-anchor from current
            // state, per this tool's description.
            TopoError::Compacted { .. } => ErrorData::invalid_params(e.to_string(), None),
            other => ErrorData::internal_error(other.to_string(), None),
        })?;
        let ops = events
            .into_iter()
            .map(|ev| {
                serde_json::to_value(ev.op.as_ref())
                    .map(|op| ChangeEventJson { seq: ev.seq, op })
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(GetChangesResult { ops }))
    }

    #[tool(
        description = "Store a new memory. Call this when the user or task produces information worth remembering later. content becomes the full-text-searchable body; props holds structured metadata (strings/numbers/bools). Returns the new node's id — keep it if you plan to link this memory to entities."
    )]
    fn create_memory(
        &self,
        Parameters(p): Parameters<CreateMemoryParams>,
    ) -> Result<Json<CreateResult>, ErrorData> {
        let props = convert::merge_required_prop(
            MEMORY_CONTENT_PROP,
            PropValue::Str(p.content),
            p.props.as_ref(),
        )
        .map_err(|e| ErrorData::invalid_params(e, None))?;
        let scope = self.resolve_scope(p.scope.as_deref())?;
        let id = NodeId::new();
        self.submit_write(vec![Op::CreateNode {
            id,
            scope,
            label: MEMORY_LABEL.into(),
            props,
        }])?;
        Ok(Json(CreateResult { id: id.to_string() }))
    }

    #[tool(
        description = "Create an entity node (person, project, concept). Call this the FIRST time something is mentioned that memories should attach to; use find_by_prop first to check it doesn't already exist. name is equality-indexed for exact lookup."
    )]
    fn create_entity(
        &self,
        Parameters(p): Parameters<CreateEntityParams>,
    ) -> Result<Json<CreateResult>, ErrorData> {
        let props = convert::merge_required_prop(
            ENTITY_NAME_PROP,
            PropValue::Str(p.name),
            p.props.as_ref(),
        )
        .map_err(|e| ErrorData::invalid_params(e, None))?;
        let scope = self.resolve_scope(p.scope.as_deref())?;
        let id = NodeId::new();
        self.submit_write(vec![Op::CreateNode {
            id,
            scope,
            label: ENTITY_LABEL.into(),
            props,
        }])?;
        Ok(Json(CreateResult { id: id.to_string() }))
    }

    #[tool(
        description = "Create a typed, time-aware edge between two existing nodes. Call this to connect a memory to the entities it concerns, or entities to each other (e.g. 'works_on'). edge_type is free-form but be consistent — traverse can filter by it. Returns the edge id. Errors if either node doesn't exist."
    )]
    fn link(&self, Parameters(p): Parameters<LinkParams>) -> Result<Json<LinkResult>, ErrorData> {
        let from = parse_node_id(&p.from_id)?;
        let to = parse_node_id(&p.to_id)?;
        let props = match &p.props {
            Some(v) => convert::json_to_props(v).map_err(|e| ErrorData::invalid_params(e, None))?,
            None => Props::new(),
        };
        // `link` has no `scope` param on the wire (see `LinkParams`) — always
        // resolves through the server's configured default scope.
        let scope = self.resolve_scope(None)?;
        let id = EdgeId::new();
        self.submit_write(vec![Op::CreateEdge {
            id,
            scope,
            ty: p.edge_type.into(),
            from,
            to,
            props,
            valid_from: p.valid_from,
        }])?;
        Ok(Json(LinkResult { id: id.to_string() }))
    }

    #[tool(
        description = "Set or remove properties on an existing node. In `props`, a null value REMOVES that key; any other scalar sets it. Errors if the node doesn't exist. Returns the committed seq."
    )]
    fn set_node_props(
        &self,
        Parameters(p): Parameters<SetNodePropsParams>,
    ) -> Result<Json<SeqResult>, ErrorData> {
        let id = parse_node_id(&p.id)?;
        let props = convert::json_to_prop_changes(&p.props)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let seq = self.submit_seq(vec![Op::SetNodeProps { id, props }])?;
        Ok(Json(SeqResult { seq }))
    }

    #[tool(
        description = "Hard-delete a node and cascade-remove its incident edges. Call this to forget something entirely. Errors if the node doesn't exist. Returns the committed seq."
    )]
    fn remove_node(
        &self,
        Parameters(p): Parameters<RemoveNodeParams>,
    ) -> Result<Json<SeqResult>, ErrorData> {
        let id = parse_node_id(&p.id)?;
        let seq = self.submit_seq(vec![Op::RemoveNode { id }])?;
        Ok(Json(SeqResult { seq }))
    }

    #[tool(
        description = "Close an open edge, stamping its valid_to. valid_to defaults to now when omitted. Errors if the edge doesn't exist. Returns the committed seq."
    )]
    fn close_edge(
        &self,
        Parameters(p): Parameters<CloseEdgeParams>,
    ) -> Result<Json<SeqResult>, ErrorData> {
        let id = EdgeId::from_str(&p.id).map_err(|e| {
            ErrorData::invalid_params(format!("invalid edge id {:?}: {e}", p.id), None)
        })?;
        let seq = self.submit_seq(vec![Op::CloseEdge {
            id,
            valid_to: p.valid_to,
        }])?;
        Ok(Json(SeqResult { seq }))
    }

    #[tool(
        description = "Attach a raw embedding vector to an existing node under `model`. The host computes the vector; TopoDB stores it as-is for cosine search. Errors if the node doesn't exist or the vector's dimension conflicts with the model's existing vectors. Returns the committed seq."
    )]
    fn set_embedding(
        &self,
        Parameters(p): Parameters<SetEmbeddingParams>,
    ) -> Result<Json<SeqResult>, ErrorData> {
        let id = parse_node_id(&p.id)?;
        let vector =
            convert::json_to_f32_vec(&p.vector).map_err(|e| ErrorData::invalid_params(e, None))?;
        let seq = self.submit_seq(vec![Op::SetEmbedding {
            id,
            model: p.model,
            vector,
        }])?;
        Ok(Json(SeqResult { seq }))
    }

    #[tool(
        description = "Cosine vector search under one model. The query is a raw embedding array (host-computed); TopoDB ranks stored embeddings by cosine similarity. Optionally restrict scoring to a candidate node set (for hybrid recall after a traverse). Errors if k is 0 or the vector is empty."
    )]
    fn search_vectors(
        &self,
        Parameters(p): Parameters<SearchVectorsParams>,
    ) -> Result<Json<SearchVectorsResult>, ErrorData> {
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
        let vector =
            convert::json_to_f32_vec(&p.vector).map_err(|e| ErrorData::invalid_params(e, None))?;
        let candidates = match p.candidates {
            None => None,
            Some(cs) => {
                let mut ids = Vec::with_capacity(cs.len());
                for c in &cs {
                    ids.push(parse_node_id(c)?);
                }
                Some(ids)
            }
        };
        let query = VectorQuery {
            scopes,
            model: p.model,
            vector,
            k: p.k,
            candidates,
        };
        let hits = self.db.search_vector(&query).map_err(classify_topo_error)?;
        let hits = hits
            .iter()
            .map(|(n, score)| {
                convert::node_to_json(n).map(|node| SearchHit {
                    node,
                    score: *score,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(SearchVectorsResult { hits }))
    }

    #[tool(
        description = "Submit a batch of high-level commands (a JSON array of command objects) atomically — all commit or none. Each command's \"op\" matches a tool name, but field names are the batch DSL's own (not always identical to the tool's param names) — see per-op fields below. `#N` in an id field references the id produced by the Nth earlier command (0-indexed, backward-only), e.g. create a memory and entity, then link them. Returns the produced ids in order (null for commands that create nothing). Per-op fields: create_memory { content, scope?, props? }; create_entity { name, scope?, props? }; link { from, to, type, props?, valid_from? } — note link uses from/to/type, NOT the link tool's from_id/to_id/edge_type; set_node_props { id, props } (props value null removes that key); remove_node { id }; close_edge { id, valid_to? }; set_embedding { id, model, vector }."
    )]
    fn submit_batch(
        &self,
        Parameters(p): Parameters<SubmitBatchParams>,
    ) -> Result<Json<SubmitBatchResult>, ErrorData> {
        let (ops, ids) = convert::resolve_batch(&p.commands, self.default_scope)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        self.submit_write(ops)?;
        Ok(Json(SubmitBatchResult { ids }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TopoServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo::new` defaults `server_info` to rmcp's own
        // `Implementation::from_build_env()` (reporting "rmcp"/its version), so
        // override it with this crate's identity.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "TopoDB agent-memory engine exposed over MCP: a temporal property graph with \
                 scoped recall. Tool calls that omit `scope` use the server's configured default \
                 scope (see db_info). Start with db_info to confirm wiring.",
            )
    }
}
