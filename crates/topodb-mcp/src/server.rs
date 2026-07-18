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
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, Meta, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use topodb::{
    Db, Direction, EdgeId, NodeId, Op, PropValue, Props, Scope, ScopeSet, SearchOptions, TopoError,
    TraversalQuery, VectorQuery,
};

use crate::config::{
    scope_label, Config, ReadScopes, ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP,
    MEMORY_LABEL,
};
use topodb_json as convert;

/// The MCP server state. `Clone` is required by rmcp (the service clones the
/// handler per request); every field is cheap to clone — [`Db`] is an `Arc`
/// handle, [`ScopeSet`] is a small set, and the rest are owned metadata.
#[derive(Clone)]
pub struct TopoServer {
    db: Db,
    /// The configured default **write** scope: a create/link tool call that
    /// omits `scope` is stamped with this. Reads never consult this directly —
    /// see `default_scopes` below.
    default_scope: Scope,
    /// The configured default **read** set (from `--read-scopes`, or `--scope`
    /// alone), reused by every scoped read tool call that omits `scope`/`scopes`
    /// (see [`TopoServer::resolve_scopes`]).
    default_scopes: ScopeSet,
    /// The same default read set as `default_scopes`, kept as `ReadScopes`:
    /// `ScopeSet::iter_scopes` is `pub(crate)` to `topodb`,
    /// so `db_info` (Finding 2) renders its reported read set from this list
    /// via `scope_label` rather than from `default_scopes` directly.
    default_read_scopes: ReadScopes,
    /// Rendered db path, reported by `db_info`.
    db_path: String,
    /// See `Config::allow_unscoped_changes`.
    allow_unscoped_changes: bool,
    tool_router: ToolRouter<Self>,
}

/// JSON-RPC `_meta` key carrying a **per-request** default *write* scope,
/// overriding `--scope` for that one request. Value: `"shared"` or a ULID.
pub const META_SCOPE: &str = "topodb/scope";
/// JSON-RPC `_meta` key carrying a **per-request** default *read* scope set,
/// overriding `--read-scopes` for that one request. Value: a non-empty array of
/// `"shared"` / ULID strings.
pub const META_READ_SCOPES: &str = "topodb/read_scopes";

impl TopoServer {
    /// Returns the handler this request should run against: `self`, but with the
    /// configured scope defaults replaced by any the request carried in `_meta`.
    ///
    /// WHY THIS EXISTS. `--scope`/`--read-scopes` are *process-wide* defaults,
    /// which is fine when one client owns one server process. The plugin broker
    /// breaks that assumption: redb lets only ONE process hold the database, so a
    /// single `topodb-mcp` is multiplexed across every concurrent session — and
    /// sessions in different projects need *different* scopes. Before this,
    /// whichever session happened to spawn the broker fixed `--scope` for all of
    /// them, and every later project silently read and wrote into the first
    /// project's memory. (`plugins/claude-code/test/broker.test.js`:
    /// `each_session_writes_to_its_own_project_scope`.)
    ///
    /// Scope therefore has to travel with the *request*, not with the process.
    /// `_meta` is the right carrier: it is the MCP envelope's own extension
    /// point, so the broker stamps ONE field on every request it forwards and
    /// needs to know nothing about any tool's arguments. That matters — an
    /// arguments-rewriting broker would have to know that reads take
    /// `scope`/`scopes`, writes take `scope`, and `submit_batch` takes neither
    /// (it defaults *per command*, inside `resolve_batch`), and it would silently
    /// mis-default the first tool added with a shape it didn't anticipate.
    ///
    /// Because the tool router dispatches against the handler reference we hand
    /// it, EVERY tool — `db_info` and `submit_batch` included — transparently
    /// sees these values as its defaults. No tool signature changes, and a new
    /// tool is covered the day it is written.
    ///
    /// An explicit `scope`/`scopes` *argument* still wins over these defaults,
    /// exactly as it wins over the CLI ones: this replaces the fallback, it does
    /// not pin the request. That is what keeps `scope: "shared"` working as the
    /// documented way to store a lesson that generalizes beyond one repo.
    fn for_request(&self, meta: &Meta) -> Result<Self, ErrorData> {
        let scope_v = meta.get(META_SCOPE);
        let read_v = meta.get(META_READ_SCOPES);
        // The overwhelmingly common path (a plain stdio client, no broker):
        // nothing to override, so don't pay for a clone-and-rebuild.
        if scope_v.is_none() && read_v.is_none() {
            return Ok(self.clone());
        }

        let mut out = self.clone();

        if let Some(v) = scope_v {
            let s = v.as_str().ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("`{META_SCOPE}` in _meta must be a string (\"shared\" or a ULID)"),
                    None,
                )
            })?;
            out.default_scope = convert::resolve_scope(Some(s), self.default_scope)
                .map_err(|e| ErrorData::invalid_params(e, None))?;
        }

        let read_list: Option<Vec<Scope>> = match read_v {
            Some(v) => {
                let arr = v.as_array().ok_or_else(|| {
                    ErrorData::invalid_params(
                        format!("`{META_READ_SCOPES}` in _meta must be an array of \"shared\"/ULID strings"),
                        None,
                    )
                })?;
                let resolved = arr
                    .iter()
                    .map(|x| {
                        let s = x.as_str().ok_or_else(|| {
                            format!("`{META_READ_SCOPES}` entries must be strings")
                        })?;
                        convert::resolve_scope(Some(s), out.default_scope)
                    })
                    .collect::<Result<Vec<Scope>, String>>()
                    .map_err(|e| ErrorData::invalid_params(e, None))?;
                Some(resolved)
            }
            // A request that overrides the write scope but says nothing about
            // reads must NOT keep the process-wide read set — that set belongs to
            // whichever session spawned the server, which is the very bug this
            // exists to close. Fall back the same way `config.rs` does when
            // `--read-scopes` is omitted: the read set becomes the write scope.
            None if scope_v.is_some() => Some(vec![out.default_scope]),
            None => None,
        };

        if let Some(list) = read_list {
            // Rejects the empty set, which admits nothing and is never what a
            // caller means (there is no unscoped read).
            let rs = ReadScopes::new(list)
                .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
            out.default_scopes = convert::scopes_to_scope_set(rs.as_slice());
            out.default_read_scopes = rs;
        }

        Ok(out)
    }

    /// Wraps an open [`Db`] and the resolved [`Config`] into a server handler.
    pub fn new(db: Db, config: &Config) -> Self {
        let default_scopes = convert::scopes_to_scope_set(config.default_read_scopes.as_slice());
        Self {
            db,
            default_scope: config.default_scope,
            default_scopes,
            default_read_scopes: config.default_read_scopes.clone(),
            db_path: config.db_path.display().to_string(),
            allow_unscoped_changes: config.allow_unscoped_changes,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolves a read tool's optional `scope` / `scopes` params to the
    /// [`ScopeSet`] the read runs against. Precedence:
    ///
    /// 1. `scopes` (non-empty) → a genuine multi-member set. This is the only
    ///    way a client can read across e.g. a project scope *and* `shared`.
    /// 2. `scope` → a one-member set (the pre-P1 behaviour).
    /// 3. neither → the server's configured default read set (`--read-scopes`,
    ///    or `--scope` alone), pre-resolved once in `new` rather than re-derived
    ///    on every call — the common case.
    ///
    /// An explicitly empty `scopes: []` is rejected: an empty set admits
    /// nothing, so it is a caller error, never "read everything" (there is no
    /// unscoped read).
    fn resolve_scopes(
        &self,
        scope: Option<&str>,
        scopes: Option<&[String]>,
    ) -> Result<ScopeSet, ErrorData> {
        match scopes {
            Some([]) => Err(ErrorData::invalid_params(
                "`scopes` must not be empty (an empty scope set admits nothing); \
                 omit it to use the server's default read scopes"
                    .to_string(),
                None,
            )),
            Some(list) => {
                let resolved = list
                    .iter()
                    .map(|s| convert::resolve_scope(Some(s), self.default_scope))
                    .collect::<Result<Vec<Scope>, String>>()
                    .map_err(|e| ErrorData::invalid_params(e, None))?;
                Ok(convert::scopes_to_scope_set(&resolved))
            }
            None => match scope {
                None => Ok(self.default_scopes.clone()),
                Some(_) => {
                    let resolved = convert::resolve_scope(scope, self.default_scope)
                        .map_err(|e| ErrorData::invalid_params(e, None))?;
                    Ok(convert::scope_to_scope_set(resolved))
                }
            },
        }
    }

    /// Resolves a write tool's optional `scope` param to the single [`Scope`]
    /// the created node/edge is stamped with. Unlike `resolve_scopes` (which
    /// expands to a `ScopeSet` for reads), a write needs exactly one `Scope`
    /// value, not a set to filter by — so this goes through
    /// [`convert::resolve_scope`] directly rather than also converting to a
    /// `ScopeSet`. Every write tool (`create_memory`, `create_entity`, `link`)
    /// passes its optional `scope` param through here; `None` resolves to the
    /// server's configured default write scope.
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

/// Schema stand-in for `submit_batch`'s command list: an array of objects.
type CommandsSchema = Vec<Value>;

/// The JSON Schema for a raw embedding: a non-empty array of numbers.
///
/// `minItems: 1` is the advertised half of an engine rule — `prevalidate_dims`
/// rejects a zero-dim embedding (it would otherwise fix the `(model, scope)`
/// slab's dim at 0 and block every real embedding under that key), and
/// `search_vector` rejects an empty query vector.
fn vector_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "items": { "type": "number" },
        "minItems": 1,
    })
}

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

/// Wall-clock milliseconds since the Unix epoch, read once per call site.
fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as i64
}

/// Sanity-checks an agent-supplied temporal bound (`link`'s `valid_from`,
/// `close_edge`'s `valid_to`). Two silent-failure traps are worth a hard
/// error here: a seconds-since-epoch value (any modern date is ~2e9, below
/// the 1e12 ms floor) would land the bound in January 1970, and a
/// future-dated bound makes the edge invisible to every "now" read until
/// that instant arrives — both produce an edge that LOOKS written but never
/// surfaces. 5 minutes of forward slack absorbs clock skew.
fn validate_ms_timestamp(field: &str, v: i64) -> Result<(), ErrorData> {
    const MIN_MS: i64 = 1_000_000_000_000; // 2001-09-09 in ms
    const FUTURE_SLACK_MS: i64 = 5 * 60 * 1000;
    let now = wall_clock_ms();
    if v < MIN_MS {
        return Err(ErrorData::invalid_params(
            format!(
                "{field} = {v} is not a plausible milliseconds-since-epoch value \
                 (below {MIN_MS}). This looks like SECONDS since the epoch — \
                 multiply by 1000."
            ),
            None,
        ));
    }
    if v > now + FUTURE_SLACK_MS {
        return Err(ErrorData::invalid_params(
            format!(
                "{field} = {v} is in the future (now = {now} ms). A future-dated \
                 bound makes the edge invisible to every \"now\" traversal until \
                 that time arrives; pass a past-or-present ms timestamp, or omit \
                 the field to let the engine stamp commit time."
            ),
            None,
        ));
    }
    Ok(())
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
    /// Default WRITE scope applied to a create/link tool call that omits
    /// `scope`: `"shared"` or a ULID string. NOT the read set — see
    /// `default_read_scopes`. A read tool call that passes this value as its
    /// own `scope` narrows the read to just this one scope, which can be
    /// STRICTER than the default read set below.
    default_scope: String,
    /// Default READ scope set applied to a read tool call that omits both
    /// `scope` and `scopes` (from `--read-scopes`, or `--scope` alone):
    /// `"shared"` and/or ULID strings. Distinct from `default_scope` — a read
    /// filters by this whole set, a write is stamped with the single
    /// `default_scope` above.
    default_read_scopes: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetNodeParams {
    /// ULID of the node to fetch.
    id: String,
    /// Scope to look the node up in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present — an empty set admits nothing (there is no
    /// unscoped read); `minItems: 1` is the advertised half of that rule, see
    /// `resolve_scopes`'s `Some([])` rejection for the runtime half.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
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
#[serde(deny_unknown_fields)]
struct FindByPropParams {
    /// Node label to match, e.g. `"Entity"`.
    label: String,
    /// Property name to match — must be declared in the index spec's
    /// equality list for this label.
    prop: String,
    /// Value to match: a string, integer, or boolean (floats are not
    /// equality-indexable). String matching is case- and whitespace-
    /// insensitive unless `exact` is set.
    #[schemars(schema_with = "prop_value_schema")]
    value: Value,
    /// Require a byte-exact value match. Defaults to `false`: string values
    /// match case- and whitespace-insensitively ("drew powell" finds
    /// "Drew Powell"), which is almost always what a dedup or resolve step
    /// wants.
    #[serde(default)]
    exact: bool,
    /// Scope to search in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present — an empty set admits nothing (there is no
    /// unscoped read); `minItems: 1` is the advertised half of that rule, see
    /// `resolve_scopes`'s `Some([])` rejection for the runtime half.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
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

fn default_recency_weight() -> f32 {
    0.3
}

fn default_recency_half_life_days() -> f64 {
    30.0
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present — an empty set admits nothing (there is no
    /// unscoped read); `minItems: 1` is the advertised half of that rule, see
    /// `resolve_scopes`'s `Some([])` rejection for the runtime half.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
    /// How much recency shifts ranking, 0.0-1.0. Each hit's BM25 score is
    /// multiplied by `(1-w) + w * 2^(-age/half_life)` (age = time since the
    /// node was created), so fresher memories win ties and stale ones sink
    /// without a strong old match ever being erased. Set 0 for pure BM25.
    #[serde(default = "default_recency_weight")]
    #[schemars(range(min = 0.0, max = 1.0))]
    recency_weight: f32,
    /// Half-life for the recency decay, in days. Must be > 0.
    #[serde(default = "default_recency_half_life_days")]
    #[schemars(range(min = 0.001))]
    recency_half_life_days: f64,
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
#[serde(deny_unknown_fields)]
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
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present — an empty set admits nothing (there is no
    /// unscoped read); `minItems: 1` is the advertised half of that rule, see
    /// `resolve_scopes`'s `Some([])` rejection for the runtime half.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct TraverseResult {
    /// `{"nodes": [...], "edges": [...]}` reached from the seed.
    subgraph: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AccessStatsParams {
    /// ULID of the node.
    id: String,
    /// Scope to look the node up in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present — an empty set admits nothing (there is no
    /// unscoped read); `minItems: 1` is the advertised half of that rule, see
    /// `resolve_scopes`'s `Some([])` rejection for the runtime half.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Result of a find-or-create write (`create_entity`).
#[derive(Debug, Serialize, JsonSchema)]
struct UpsertResult {
    /// ULID of the entity: newly created when `created` is true, the
    /// already-existing node's id otherwise.
    id: String,
    /// `false` means the name resolved (case/whitespace-insensitively) to an
    /// existing entity and NO new node was created — link against this id.
    created: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LinkParams {
    /// ULID of the edge's source (`from`) node. Must already exist.
    from_id: String,
    /// ULID of the edge's target (`to`) node. Must already exist.
    to_id: String,
    /// Free-form edge type (e.g. `"works_on"`, `"about"`). Be consistent —
    /// `traverse` can filter by it.
    edge_type: String,
    /// Scope to create the edge in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted. Set this explicitly
    /// when linking nodes that live in a scope other than the default —
    /// otherwise the edge is stamped with the default scope and is invisible
    /// to readers of the nodes' own scope.
    #[serde(default)]
    scope: Option<String>,
    /// Structured metadata on the edge (string/number/bool values).
    #[serde(default)]
    #[schemars(with = "Option<PropsSchema>")]
    props: Option<Value>,
    /// Milliseconds since Unix epoch the edge becomes valid from. Defaults to
    /// "now" (resolved by the engine at commit time) when omitted. Must be a
    /// plausible past-or-present ms value — seconds-since-epoch and
    /// future-dated values are rejected (both would make the edge invisible
    /// or wrongly dated).
    #[serde(default)]
    valid_from: Option<i64>,
    /// The new fact REPLACES the old one for this relation: atomically close
    /// every other open edge of the same type from this node (to any other
    /// target) before creating/reusing this one. Use for to-one relations
    /// whose target changed — e.g. `works_at` a new employer. Leave false
    /// (the default) for relations that accumulate (`knows`, `about`).
    #[serde(default)]
    supersede: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LinkResult {
    /// ULID of the edge: newly created when `created` is true, the existing
    /// open edge with the same from/to/type otherwise.
    id: String,
    /// `false` means an identical open edge already existed and was reused —
    /// no duplicate was created.
    created: bool,
    /// Edge ids closed by `supersede: true` (empty otherwise).
    superseded: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GetEdgesParams {
    /// ULID of the source node whose outgoing edges to list.
    from_id: String,
    /// Restrict to edges pointing at this target node ULID.
    #[serde(default)]
    to_id: Option<String>,
    /// Restrict to this edge type (normalized like `link` normalizes it;
    /// edges stored under the raw un-normalized form are matched too).
    #[serde(default)]
    edge_type: Option<String>,
    /// Only currently-open edges (no `valid_to`). Defaults to true — the
    /// common case is finding the open edge that a changed fact should close.
    #[serde(default = "default_true")]
    open_only: bool,
    /// Scope to read in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetEdgesResult {
    /// Matching edges (id/type/from/to/scope/props/valid_from/valid_to),
    /// oldest first. `valid_to: null` means the edge is currently open.
    edges: Vec<Value>,
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
#[serde(deny_unknown_fields)]
struct SetNodePropsParams {
    /// ULID of the node to update.
    id: String,
    /// Property changes: a `null` value REMOVES the key, any other scalar sets
    /// it.
    #[schemars(with = "PropsSchema")]
    props: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveNodeParams {
    /// ULID of the node to hard-delete (its incident edges cascade away).
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CloseEdgeParams {
    /// ULID of the edge to close.
    id: String,
    /// Unix ms the edge becomes valid until; defaults to "now" (engine
    /// -resolved) when omitted.
    #[serde(default)]
    valid_to: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetEmbeddingParams {
    /// ULID of the node to attach the embedding to.
    id: String,
    /// Embedding model name (namespaces the vector).
    model: String,
    /// Raw embedding as a non-empty JSON array of finite numbers
    /// (host-computed).
    #[schemars(schema_with = "vector_schema")]
    vector: Value,
}

fn default_vector_k() -> usize {
    10
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchVectorsParams {
    /// Embedding model name to search within.
    model: String,
    /// Query embedding as a non-empty JSON array of finite numbers
    /// (host-computed).
    #[schemars(schema_with = "vector_schema")]
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
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes. Must not
    /// be empty when present — an empty set admits nothing (there is no
    /// unscoped read); `minItems: 1` is the advertised half of that rule, see
    /// `resolve_scopes`'s `Some([])` rejection for the runtime half.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
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
#[serde(deny_unknown_fields)]
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
        description = "Report the open database's path, current op-log sequence number, the default WRITE scope applied to a create/link call that omits scope, and the default READ scope set applied to a read call that omits both scope/scopes. Call this first to confirm the server is wired to the expected database and read set, and to obtain current_seq as the anchor for get_changes. NOTE: the default read set can be WIDER than the default write scope (e.g. --read-scopes project,shared with --scope project) — passing default_scope as a read call's own `scope` NARROWS the read to that one scope, which can be stricter than staying on the defaults."
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
            default_read_scopes: self
                .default_read_scopes
                .as_slice()
                .iter()
                .map(scope_label)
                .collect(),
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
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        match self.db.node(&scope_set, id) {
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
        description = "Look up nodes by an equality-indexed property (e.g. an Entity's name). String values match case- and whitespace-insensitively by default ('drew powell' finds 'Drew Powell'); pass exact: true for a byte-exact match. Call this to resolve a known identifier to a node — for topic/phrase search use search_memories instead. Errors if (label, prop) is not declared in the index spec. Zero rows (not an error) when nothing matches — before concluding an entity is new, also try search_memories with the name, and check the shared scope (scopes: [<project>, \"shared\"])."
    )]
    fn find_by_prop(
        &self,
        Parameters(p): Parameters<FindByPropParams>,
    ) -> Result<Json<FindByPropResult>, ErrorData> {
        let value = convert::json_to_prop_value(&p.value)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        // `nodes_by_prop` opens a redb read transaction (an on-disk
        // PROP_INDEX scan + record fetches in v3), so — like `search_text` —
        // it can fail with `Storage`/`Encoding`, not just `Rejected`
        // (undeclared index / Float value). Only the input-validation
        // `Rejected` maps to invalid_params; everything else is a
        // server-side internal_error (same split as `search_memories`).
        let hits = if p.exact {
            self.db.nodes_by_prop(&scope_set, &p.label, &p.prop, &value)
        } else {
            self.db
                .nodes_by_prop_normalized(&scope_set, &p.label, &p.prop, &value)
        }
        .map_err(|e| match e {
            TopoError::Rejected(_) => ErrorData::invalid_params(e.to_string(), None),
            other => ErrorData::internal_error(other.to_string(), None),
        })?;
        let nodes = hits
            .iter()
            .map(convert::node_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(FindByPropResult { nodes }))
    }

    #[tool(
        description = "Full-text BM25 search over indexed text (memory content AND entity names), recency-weighted: at equal relevance, fresher memories rank above stale ones (tune with recency_weight, 0 = pure BM25). Matching is exact-token after lowercasing — no stemming or fuzzy matching, so 'databases' does NOT match 'database'; if a query returns nothing useful, retry with different word forms or synonyms, raise k, or widen scopes before concluding nothing is stored. Then traverse from the best hit to gather its linked context."
    )]
    fn search_memories(
        &self,
        Parameters(p): Parameters<SearchMemoriesParams>,
    ) -> Result<Json<SearchMemoriesResult>, ErrorData> {
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        let options = SearchOptions {
            recency_weight: p.recency_weight,
            recency_half_life_ms: (p.recency_half_life_days * 86_400_000.0) as i64,
            now_ms: None,
        };
        // `search_text` opens a redb read transaction, so unlike the pure
        // snapshot reads it CAN fail with `Storage`/`Encoding` — only its
        // input-validation `Rejected` (k == 0, token-less query, bad recency
        // tuning) maps to invalid_params; everything else is a server-side
        // internal_error.
        let hits = self
            .db
            .search_text_with(&scope_set, &p.query, p.k, &options)
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
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        // Each requested type name probes BOTH its raw and normalized forms:
        // `link` normalizes on write, but edges written before normalization
        // (or by a raw engine caller) are stored verbatim — a filter that
        // only knew one form would silently drop the other's edges.
        let edge_types = p.edge_types.map(|v| {
            let mut out: Vec<_> = Vec::with_capacity(v.len());
            for name in v {
                if let Ok(norm) = convert::normalize_edge_type(&name) {
                    if norm != name {
                        out.push(norm.into());
                    }
                }
                out.push(name.into());
            }
            out
        });
        let query = TraversalQuery {
            scopes: scope_set,
            seeds: vec![seed],
            max_hops: p.max_hops,
            edge_types,
            direction: p.direction.into(),
            as_of: None,
        };
        // `traverse` opens a redb read transaction and walks on-disk chunked
        // adjacency (v3), so — like `search_text` — it can fail with
        // `Storage`/`Encoding`, not just `Rejected` (max_hops out of 1..=4).
        // Only the input-validation `Rejected` maps to invalid_params;
        // everything else is a server-side internal_error (same split as
        // `search_memories`).
        let sg = self.db.traverse(&query).map_err(|e| match e {
            TopoError::Rejected(_) => ErrorData::invalid_params(e.to_string(), None),
            other => ErrorData::internal_error(other.to_string(), None),
        })?;
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
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        let stats = self
            .db
            .access_stats(&scope_set, id)
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
        description = "Replay the operation log from a sequence number (inclusive). Host-level primitive for consolidation/sync — the ONE unscoped read; the log spans all scopes. Returns ops with their seq numbers; on Compacted errors, re-anchor from current state. The db_info tool reports current_seq. Disabled unless the server was started with --allow-unscoped-changes."
    )]
    fn get_changes(
        &self,
        Parameters(p): Parameters<GetChangesParams>,
    ) -> Result<Json<GetChangesResult>, ErrorData> {
        if !self.allow_unscoped_changes {
            return Err(ErrorData::invalid_params(
                "get_changes is disabled: it is the one unscoped read (the op log \
                 spans every scope in the db), so it is off by default. Restart \
                 topodb-mcp with --allow-unscoped-changes to enable it."
                    .to_string(),
                None,
            ));
        }
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
        description = "Store a new memory. Call this when the user or task produces information worth remembering later. content becomes the full-text-searchable body; props holds structured metadata (strings/numbers/bools). Returns the new node's id — then LINK it to the entities it concerns (create_entity + link), every time: an unlinked memory can only ever be found by keyword search, never by traversing from the people/projects it is about."
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
        description = "Find-or-create an entity node (person, project, concept). Safe to call whenever something worth attaching memories to is mentioned: the name is matched case- and whitespace-insensitively across the read scopes, the write scope, AND shared — if the entity already exists anywhere visible, its id is returned with created: false and NO duplicate is made (any new props keys are merged; existing keys are never overwritten). Use one canonical name form per entity (prefer the fullest name you know, e.g. 'Drew Powell' over 'Drew') so future mentions keep resolving to the same node."
    )]
    fn create_entity(
        &self,
        Parameters(p): Parameters<CreateEntityParams>,
    ) -> Result<Json<UpsertResult>, ErrorData> {
        let props = convert::merge_required_prop(
            ENTITY_NAME_PROP,
            PropValue::Str(p.name.clone()),
            p.props.as_ref(),
        )
        .map_err(|e| ErrorData::invalid_params(e, None))?;
        let scope = self.resolve_scope(p.scope.as_deref())?;

        // Dedup lookup runs against everything this session can SEE plus
        // everything it could COLLIDE with: the default read set, the write
        // scope, and shared. Without shared here, a shared entity would be
        // invisible to a project-scoped check and get a project-local twin —
        // the single most common duplicate-entity path.
        let mut lookup_scopes: Vec<Scope> = self.default_read_scopes.as_slice().to_vec();
        lookup_scopes.push(scope);
        lookup_scopes.push(Scope::Shared);
        let lookup_set = convert::scopes_to_scope_set(&lookup_scopes);
        let existing = match self.db.nodes_by_prop_normalized(
            &lookup_set,
            ENTITY_LABEL,
            ENTITY_NAME_PROP,
            &PropValue::Str(p.name.clone()),
        ) {
            // Oldest id wins (ULIDs sort by mint time): when duplicates
            // already exist from before upsert semantics, every new link
            // converges on one canonical node instead of scattering further.
            Ok(hits) => hits.into_iter().min_by_key(|n| n.id),
            // A custom spec without (Entity, name) equality-indexed can't
            // dedup — degrade to create-always rather than failing the write.
            Err(TopoError::Rejected(_)) => None,
            Err(e) => return Err(classify_topo_error(e)),
        };

        if let Some(node) = existing {
            // Merge only NEW metadata keys onto the existing entity; never
            // overwrite what's already recorded, and never touch `name` (the
            // stored casing stays canonical).
            let new_keys: std::collections::BTreeMap<String, Option<PropValue>> = props
                .into_iter()
                .filter(|(k, _)| k != ENTITY_NAME_PROP && !node.props.contains_key(k))
                .map(|(k, v)| (k, Some(v)))
                .collect();
            if !new_keys.is_empty() {
                self.submit_write(vec![Op::SetNodeProps {
                    id: node.id,
                    props: new_keys,
                }])?;
            }
            return Ok(Json(UpsertResult {
                id: node.id.to_string(),
                created: false,
            }));
        }

        let id = NodeId::new();
        self.submit_write(vec![Op::CreateNode {
            id,
            scope,
            label: ENTITY_LABEL.into(),
            props,
        }])?;
        Ok(Json(UpsertResult {
            id: id.to_string(),
            created: true,
        }))
    }

    #[tool(
        description = "Create (or reuse) a typed, time-aware edge between two existing nodes. ALWAYS link what you store: a memory to the entities it concerns ('about'), entities to each other ('works_on', 'works_at') — an unlinked memory is invisible to traverse. edge_type is normalized (lowercased; spaces/hyphens collapse to '_', so 'Works At' == 'works_at'); reuse existing type names rather than inventing synonyms ('works_at', not also 'employed_by'). Calling link again with the same from/to/type returns the existing open edge (created: false) instead of a duplicate. When the new fact REPLACES the old one for a to-one relation (moved teams, changed employer), pass supersede: true to atomically close the other open same-type edges from this node. Errors if either node doesn't exist. When linking shared-scope nodes, pass scope: 'shared' or the edge is invisible outside this project."
    )]
    fn link(&self, Parameters(p): Parameters<LinkParams>) -> Result<Json<LinkResult>, ErrorData> {
        let from = parse_node_id(&p.from_id)?;
        let to = parse_node_id(&p.to_id)?;
        let ty = convert::normalize_edge_type(&p.edge_type)
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        if let Some(vf) = p.valid_from {
            validate_ms_timestamp("valid_from", vf)?;
        }
        let props = match &p.props {
            Some(v) => convert::json_to_props(v).map_err(|e| ErrorData::invalid_params(e, None))?,
            None => Props::new(),
        };
        let scope = self.resolve_scope(p.scope.as_deref())?;
        let write_set = convert::scope_to_scope_set(scope);

        // Reuse an identical open edge instead of stacking a parallel
        // duplicate — re-recording a still-true fact is normal agent
        // behavior, and must be idempotent. Dedup is per write scope: a
        // deliberately different-scoped edge between the same nodes stays
        // possible.
        let existing = self
            .db
            .edges_from(&write_set, from, Some(to), Some(&ty), true)
            .map_err(classify_topo_error)?;

        let mut ops: Vec<Op> = Vec::new();
        let mut superseded: Vec<String> = Vec::new();
        if p.supersede {
            let open_same_ty = self
                .db
                .edges_from(&write_set, from, None, Some(&ty), true)
                .map_err(classify_topo_error)?;
            for e in open_same_ty.iter().filter(|e| e.to != to) {
                ops.push(Op::CloseEdge {
                    id: e.id,
                    valid_to: None,
                });
                superseded.push(e.id.to_string());
            }
        }

        if let Some(e) = existing.first() {
            // Same-target open edge already records this fact — close the
            // superseded siblings (if any) and reuse it.
            if !ops.is_empty() {
                self.submit_write(ops)?;
            }
            return Ok(Json(LinkResult {
                id: e.id.to_string(),
                created: false,
                superseded,
            }));
        }

        let id = EdgeId::new();
        ops.push(Op::CreateEdge {
            id,
            scope,
            ty: ty.into(),
            from,
            to,
            props,
            valid_from: p.valid_from,
        });
        // One submit: the closes and the create commit atomically — a
        // supersede can never close the old fact and then fail to record the
        // new one.
        self.submit_write(ops)?;
        Ok(Json(LinkResult {
            id: id.to_string(),
            created: true,
            superseded,
        }))
    }

    #[tool(
        description = "List a node's outgoing edges, optionally filtered by target node and/or edge type; open edges only by default. This is how you find the edge id to close_edge when a fact stops being true, and how you check what a node is already linked to before adding more. Returns full edge records (id, type, from, to, valid_from, valid_to) — valid_to: null means currently open."
    )]
    fn get_edges(
        &self,
        Parameters(p): Parameters<GetEdgesParams>,
    ) -> Result<Json<GetEdgesResult>, ErrorData> {
        let from = parse_node_id(&p.from_id)?;
        let to = match &p.to_id {
            Some(s) => Some(parse_node_id(s)?),
            None => None,
        };
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        let mut edges = match &p.edge_type {
            None => self
                .db
                .edges_from(&scope_set, from, to, None, p.open_only)
                .map_err(classify_topo_error)?,
            Some(raw) => {
                let norm = convert::normalize_edge_type(raw)
                    .map_err(|e| ErrorData::invalid_params(e, None))?;
                let mut es = self
                    .db
                    .edges_from(&scope_set, from, to, Some(&norm), p.open_only)
                    .map_err(classify_topo_error)?;
                // Edges written before type normalization are stored under
                // the raw form — probe it too so they stay findable.
                if norm != *raw {
                    es.extend(
                        self.db
                            .edges_from(&scope_set, from, to, Some(raw), p.open_only)
                            .map_err(classify_topo_error)?,
                    );
                }
                es
            }
        };
        edges.sort_by_key(|e| e.id);
        edges.dedup_by_key(|e| e.id);
        let edges = edges
            .iter()
            .map(convert::edge_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(GetEdgesResult { edges }))
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
        description = "Close an open edge, stamping its valid_to — the edge stops being 'currently true' but stays in history. Call this when a linked fact stops holding (left the team, project ended); find the edge id with get_edges. valid_to defaults to now when omitted (recommended). For the common 'X changed to Y' case, prefer link with supersede: true, which closes and re-links atomically. Errors if the edge doesn't exist or is already closed."
    )]
    fn close_edge(
        &self,
        Parameters(p): Parameters<CloseEdgeParams>,
    ) -> Result<Json<SeqResult>, ErrorData> {
        let id = EdgeId::from_str(&p.id).map_err(|e| {
            ErrorData::invalid_params(format!("invalid edge id {:?}: {e}", p.id), None)
        })?;
        if let Some(vt) = p.valid_to {
            validate_ms_timestamp("valid_to", vt)?;
        }
        let seq = self.submit_seq(vec![Op::CloseEdge {
            id,
            valid_to: p.valid_to,
        }])?;
        Ok(Json(SeqResult { seq }))
    }

    #[tool(
        description = "Attach a raw embedding vector to an existing node under `model`. The host computes the vector; TopoDB stores it as-is for cosine search. Errors if the node doesn't exist, the vector is empty, or its dimension conflicts with the model's existing vectors. Returns the committed seq."
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
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
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
            scopes: scope_set,
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
        description = "Submit a batch of high-level commands (a JSON array of command objects) atomically — all commit or none. Each command's \"op\" matches a tool name, but field names are the batch DSL's own (not always identical to the tool's param names) — see per-op fields below. `#N` in an id field references the id produced by the Nth earlier command (0-indexed, backward-only), e.g. create a memory and entity, then link them. Returns the produced ids in order (null for commands that create nothing). CAUTION: batch commands are raw writes — batch create_entity ALWAYS creates a new node (no find-or-create) and batch link never dedupes; when the entity or edge might already exist, use the create_entity/link tools instead. Per-op fields: create_memory { content, scope?, props? }; create_entity { name, scope?, props? }; create_node { label, props?, scope? } — a node with an arbitrary label (for host-level schemas like episode recording); link { from, to, type, scope?, props?, valid_from? } — note link uses from/to/type, NOT the link tool's from_id/to_id/edge_type; set_node_props { id, props } (props value null removes that key); remove_node { id }; close_edge { id, valid_to? }; set_embedding { id, model, vector }."
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
                 scoped recall. Reads filter by a SET of scopes (per-call `scopes: string[]`, \
                 or the server's default read set when omitted); a write is stamped with \
                 exactly ONE scope (per-call `scope: string`, or the server's default write \
                 scope when omitted). The default read set can be WIDER than the default write \
                 scope. Start with db_info to confirm wiring — it reports both defaults \
                 separately. Storing well: create_entity is find-or-create (safe to call \
                 repeatedly; never duplicates), link is idempotent per (from, to, type) and \
                 takes supersede: true when a to-one fact changes, and every memory should be \
                 linked to the entities it concerns. Recalling well: search_memories matches \
                 exact tokens only (no stemming) — retry with other word forms before \
                 concluding nothing is stored — then traverse from the best hit; use \
                 get_edges to inspect or retire a node's current relations.",
            )
    }

    /// Overrides the `#[tool_handler]`-generated `call_tool` (the macro only
    /// generates one when the impl does not already define it) so that a request
    /// carrying scope overrides in `_meta` is dispatched against a handler whose
    /// *defaults* are that request's — see [`TopoServer::for_request`].
    ///
    /// This is the ONLY place the override is applied, deliberately: the router
    /// hands each tool the `&self` we pass here, so every tool picks the session's
    /// scope up through the defaults it already reads. Doing it per-tool instead
    /// would mean 16 signatures to change and a 17th to forget.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        // MUST read from `context.meta`, NOT `request.meta`: rmcp's own
        // `ToolCallContext::new` destructures `CallToolRequestParams { meta: _, .. }`
        // and throws the request's copy away. The service layer has already swapped
        // the wire `_meta` into the RequestContext (rmcp `service.rs`), which is the
        // copy that survives.
        let session = self.for_request(&context.meta)?;
        let tcc = ToolCallContext::new(&session, request, context);
        session.tool_router.call(tcc).await
    }
}
