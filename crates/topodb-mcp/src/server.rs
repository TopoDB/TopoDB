//! The rmcp server handler wrapping a TopoDB [`Db`].
//!
//! Built on rmcp 2.2.0: the tool surface is declared with `#[tool_router]` +
//! `#[tool]` and dispatched through `#[tool_handler]` on the [`ServerHandler`]
//! impl. Task 4 adds the six read tools below (`get_node`, `find_by_prop`,
//! `search_memories`, `traverse`, `access_stats`, `get_changes`), following
//! the `db_info` pattern established in Task 3. Every tool resolves its
//! optional `scope` param via [`TopoServer::resolve_scopes`] and maps engine
//! `Err`s to `ErrorData` through `crate::convert` — never panics.

use std::str::FromStr;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use topodb::{Db, Direction, NodeId, Scope, ScopeSet, TopoError, TraversalQuery};

use crate::config::{scope_label, Config};
use crate::convert;

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
    /// Maximum number of results to return.
    #[serde(default = "default_search_k")]
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
    /// BM25 relevance score (higher is more relevant).
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
    /// Hop budget (1-4).
    #[serde(default = "default_max_hops")]
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
        let hits = self
            .db
            .search_text(&scopes, &p.query, p.k)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
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
