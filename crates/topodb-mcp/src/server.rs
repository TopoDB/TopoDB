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

use std::collections::HashSet;
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
    Db, Direction, EdgeId, NodeId, Op, PropValue, Props, RecallQuery, Scope, ScopeSet,
    SearchOptions, TopoError, TraversalQuery, VectorQuery,
};

use crate::config::{
    scope_label, Config, ReadScopes, ALIAS_EDGE_TYPE, ALIAS_LABEL, ALIAS_NAME_PROP, ENTITY_LABEL,
    ENTITY_NAME_PROP, MEMORY_CONTENT_PROP, MEMORY_LABEL, SYNONYM_EXPANSION_PROP, SYNONYM_LABEL,
    SYNONYM_TERM_PROP,
};
use crate::embedder::{Embedder, EmbedderStatus};
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
    /// The embedding subsystem's lifecycle handle — reported via `db_info`
    /// (Task 10) and consulted by every write tool that indexes text
    /// (`embed_op`, Task 11) to attach a `SetEmbedding` op when the model is
    /// `Ready`, and by `search_memories`/`recall`-backed tools to embed the
    /// query for the vector leg. A model that is not yet `Ready` (or errors
    /// on a given text) simply yields no vector for that call — writes and
    /// searches proceed text-only, and the backfill pass catches missed
    /// embeddings up once the model becomes `Ready`.
    embedder: Embedder,
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

    /// Wraps an open [`Db`], the resolved [`Config`], and the process's
    /// [`Embedder`] handle into a server handler.
    pub fn new(db: Db, config: &Config, embedder: Embedder) -> Self {
        let default_scopes = convert::scopes_to_scope_set(config.default_read_scopes.as_slice());
        Self {
            db,
            default_scope: config.default_scope,
            default_scopes,
            default_read_scopes: config.default_read_scopes.clone(),
            db_path: config.db_path.display().to_string(),
            allow_unscoped_changes: config.allow_unscoped_changes,
            embedder,
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

    /// The SetEmbedding op for `text` under the active model — or None
    /// when the embedder isn't Ready / errored on this text. Callers
    /// append it to their write batch; absence never blocks the write
    /// (backfill catches up later).
    fn embed_op(&self, id: NodeId, text: &str) -> Option<Op> {
        let vector = self.embedder.embed(text)?;
        Some(Op::SetEmbedding {
            id,
            model: self.embedder.model_name(),
            vector,
        })
    }

    /// Canonical entities for `name`: direct (Entity, name) matches plus
    /// (Alias, name) matches followed through alias_of. Deduped by id,
    /// oldest first.
    ///
    /// Returns the raw `TopoError` (not `ErrorData`) rather than swallowing
    /// it: the two existing call sites disagree on what an undeclared
    /// (Entity, name) index should mean. `find_by_prop` must still surface it
    /// as a caller error — that is the exact contract
    /// `tests/spec_persistence.rs` pins down (an undeclared-index probe on a
    /// custom spec must error, not silently return empty, or a clobbered
    /// spec reopen would go undetected). `create_entity` instead treats it as
    /// "can't dedup on this spec" and degrades to create-always. Only the
    /// (Alias, name) probe's `Rejected` is unconditionally swallowed here —
    /// a spec that predates Task 8's Alias index (or a custom spec that
    /// never declared it) simply has no aliases to resolve, which is never a
    /// caller error.
    fn resolve_entities_by_name(
        &self,
        scopes: &ScopeSet,
        name: &str,
    ) -> Result<Vec<topodb::NodeRecord>, TopoError> {
        convert::resolve_entities_by_name(&self.db, scopes, name)
    }

    /// Find-or-create lookup shared by `create_entity` and `remember`.
    ///
    /// The lookup set is everything this session can SEE plus everything it
    /// could COLLIDE with: the default read set, the write scope, and shared.
    /// Without shared here, a shared entity would be invisible to a
    /// project-scoped check and get a project-local twin — the single most
    /// common duplicate-entity path.
    ///
    /// Oldest id wins (ULIDs sort by mint time): when duplicates already
    /// exist from before upsert semantics, every new link converges on one
    /// canonical node instead of scattering further. Resolves through any
    /// alias registered for `name`, so an alias mention finds the canonical
    /// entity rather than minting a duplicate.
    ///
    /// `Ok(None)` means "create it" — covering both no-visible-match and a
    /// custom spec without the (Entity, name) equality index (`Rejected`),
    /// which degrades to create-always rather than failing the write.
    fn find_existing_entity(
        &self,
        write_scope: Scope,
        name: &str,
    ) -> Result<Option<topodb::NodeRecord>, ErrorData> {
        let mut lookup_scopes: Vec<Scope> = self.default_read_scopes.as_slice().to_vec();
        lookup_scopes.push(write_scope);
        lookup_scopes.push(Scope::Shared);
        let lookup = convert::scopes_to_scope_set(&lookup_scopes);
        convert::find_existing_entity(&self.db, &lookup, name).map_err(classify_topo_error)
    }

    /// The id of a Memory in `write_scope` whose normalized content equals
    /// `content`, if one is already stored. Dedup is scoped to the write scope
    /// only — the same fact in two projects is two memories. Looks up by the
    /// equality-indexed `content_hash` then verifies exact normalized content
    /// on each candidate, so a hash collision can never merge distinct facts.
    /// Oldest id wins if (astronomically) more than one true match exists.
    fn existing_memory(
        &self,
        write_scope: Scope,
        content: &str,
    ) -> Result<Option<NodeId>, ErrorData> {
        convert::existing_memory(&self.db, write_scope, content).map_err(classify_topo_error)
    }

    /// Ops that mark the given memory ids superseded and disconnect them from
    /// the graph, plus the ids actually marked. Each id must be a Memory in the
    /// write scope. Marking sets `superseded_at` (recall then drops it as of
    /// now, preserving `as_of`-past visibility) and closes its open out-edges
    /// (so open traversal skips it). An already-superseded id is a no-op, not
    /// re-stamped. Ops are meant to ride in the same atomic batch as the new
    /// memory, so the replacement and the retirement commit together.
    fn supersede_ops(
        &self,
        write_scope: Scope,
        ids: &[String],
    ) -> Result<(Vec<Op>, Vec<String>), ErrorData> {
        let mut ops = Vec::new();
        let mut marked = Vec::new();
        if ids.is_empty() {
            return Ok((ops, marked));
        }
        let now = now_ms();
        let scope_set = convert::scopes_to_scope_set(&[write_scope]);
        let mut seen = std::collections::BTreeSet::new();
        for raw in ids {
            let id = parse_node_id(raw)?;
            if !seen.insert(id) {
                continue;
            }
            let node = self.db.node(&scope_set, id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("supersedes id {raw} is not a node in the write scope"),
                    None,
                )
            })?;
            if node.label != MEMORY_LABEL {
                return Err(ErrorData::invalid_params(
                    format!("supersedes id {raw} is a {}, not a Memory", node.label),
                    None,
                ));
            }
            // Idempotent: an already-superseded memory is left as-is.
            if node.props.contains_key(convert::MEMORY_SUPERSEDED_AT_PROP) {
                continue;
            }
            // SetNodeProps takes `Option<PropValue>` per key (None removes).
            let mut props: std::collections::BTreeMap<String, Option<PropValue>> =
                std::collections::BTreeMap::new();
            props.insert(
                convert::MEMORY_SUPERSEDED_AT_PROP.into(),
                Some(PropValue::Int(now)),
            );
            ops.push(Op::SetNodeProps { id, props });
            for e in self
                .db
                .edges_from(&scope_set, id, None, None, true)
                .map_err(classify_topo_error)?
            {
                ops.push(Op::CloseEdge {
                    id: e.id,
                    valid_to: None,
                });
            }
            marked.push(id.to_string());
        }
        Ok((ops, marked))
    }

    /// Existing memories in `write_scope` semantically close to the just-embedded
    /// content (cosine `>=` [`NEAR_DUP_THRESHOLD`]), most-similar first, at most
    /// [`NEAR_DUP_K`]. Advisory only — the caller judges whether a hit is truly
    /// the same fact. Empty when `embedding` is `None` (embedder not Ready): no
    /// semantic signal, so no guessing. Superseded memories are skipped (already
    /// retired), as are non-Memory nodes. Called BEFORE the new memory is
    /// written, so it never returns the node being created. A vector-search
    /// error degrades to empty rather than failing the write — this is a hint.
    fn near_duplicates(
        &self,
        write_scope: Scope,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Vec<NearDuplicate> {
        let Some(vector) = embedding else {
            return Vec::new();
        };
        let query = VectorQuery {
            scopes: convert::scopes_to_scope_set(&[write_scope]),
            model: self.embedder.model_name(),
            vector: vector.to_vec(),
            k: NEAR_DUP_K,
            candidates: None,
        };
        let Ok(hits) = self.db.search_vector(&query) else {
            return Vec::new();
        };
        hits.into_iter()
            .filter(|(n, score)| {
                *score >= NEAR_DUP_REVIEW
                    && n.label == MEMORY_LABEL
                    && !n.props.contains_key(convert::MEMORY_SUPERSEDED_AT_PROP)
            })
            .map(|(n, score)| {
                let existing = match n.props.get(MEMORY_CONTENT_PROP) {
                    Some(PropValue::Str(c)) => c.clone(),
                    _ => String::new(),
                };
                NearDuplicate {
                    id: n.id.to_string(),
                    similarity: score,
                    band: dup_band(score).to_string(),
                    relation: dup_relation(content, &existing).to_string(),
                    content: existing,
                }
            })
            .collect()
    }
}

/// Cosine-similarity floor for surfacing a semantic near-duplicate.
///
/// Calibrated against the default model (bge-small-en-v1.5): the same fact in
/// different words scores ~0.83, an unrelated fact well under 0.5, so 0.80
/// catches near-duplicates while staying clear of the noise floor. Not set
/// higher because the model compresses "same fact" to ~0.83, not 0.95+; and
/// every hit is only advisory, so a borderline false positive costs the caller
/// a glance, not data.
const NEAR_DUP_THRESHOLD: f32 = 0.80;
/// How many near-duplicates to surface at most — enough to notice a redundancy
/// without burying the caller.
const NEAR_DUP_K: usize = 3;

/// Cosine floor below `NEAR_DUP_THRESHOLD` at which a pair is still surfaced, but
/// only as a weaker `"possible"` candidate. Measurement showed genuine reworded
/// duplicates can sit as low as ~0.70 — and merely-related facts sit right there
/// too (0.69), so there is NO floor that catches the former without the latter.
/// Set just under that overlap (0.68) so borderline restatements are SURFACED
/// for the caller (an LLM, a native entailment judge) to confirm, rather than
/// silently dropped; the `"possible"` band is the warning that these need a look,
/// not an automatic merge. Widening recall at the cost of precision is the right
/// trade for an advisory tool where a human/agent makes the final call.
const NEAR_DUP_REVIEW: f32 = 0.68;

/// Confidence band for a near-dup similarity: `"likely"` at/above the strong
/// floor ([`NEAR_DUP_THRESHOLD`]), `"possible"` in the review band below it.
fn dup_band(similarity: f32) -> &'static str {
    if similarity >= NEAR_DUP_THRESHOLD {
        "likely"
    } else {
        "possible"
    }
}

/// Words that retract or flip a token — the signal that separates a
/// *contradiction* (one fact superseding another) from a *duplicate*: sentence
/// embeddings score "X is A" and "X is now B, not A" as MORE similar than a
/// genuine restatement, so cosine alone can never tell them apart, but the cue
/// can. Split by which way they govern:
/// - PRE cues govern the tokens AFTER them ("not redb", "no longer windows").
const DUP_FWD_CUES: &[&str] = &[
    "not", "never", "no", "longer", "instead", "without", "rather", "over", "versus", "vs",
    "replaced", "replaces", "removed", "remove",
];
/// - POST cues govern the token immediately BEFORE them ("windows dropped",
///   "redb backend removed"). Without these, a post-nominal negation reads as an
///   assertion, so "windows dropped" and "no longer windows" — which AGREE —
///   would be mislabeled a contradiction.
const DUP_BWD_CUES: &[&str] = &[
    "dropped",
    "drops",
    "removed",
    "remove",
    "gone",
    "deprecated",
    "retired",
    "stopped",
    "killed",
    "disabled",
    "discontinued",
    "replaced",
    "replaces",
];

/// Function words (and a few high-frequency verbs) dropped before comparing
/// content tokens, so overlap reflects the salient nouns, not scaffolding.
const DUP_STOP: &[&str] = &[
    "a", "an", "the", "of", "to", "in", "on", "for", "its", "it", "is", "are", "as", "by", "with",
    "and", "or", "now", "only", "both", "this", "that", "using", "use", "uses", "chose", "runs",
    "run", "was", "were", "be", "been", "their", "them", "people", "up",
];

/// How many tokens after a PRE cue are treated as governed by it (POST cues take
/// just the one content token before them, to avoid over-negating).
const DUP_FWD_WINDOW: usize = 4;

fn dup_is_cue(w: &str) -> bool {
    DUP_FWD_CUES.contains(&w) || DUP_BWD_CUES.contains(&w)
}

fn dup_singularize(t: &str) -> &str {
    if t.len() > 3 && t.ends_with('s') {
        &t[..t.len() - 1]
    } else {
        t
    }
}

/// Tokenize `s` (lowercased alphanumeric runs) into (asserted, negated) content
/// sets: `negated` = content tokens governed by a cue (PRE cues take the tokens
/// after them within [`DUP_FWD_WINDOW`]; POST cues take the nearest content token
/// before them); `asserted` = every other content token. Stopwords and cues are
/// dropped.
fn dup_analyze(s: &str) -> (HashSet<String>, HashSet<String>) {
    let toks: Vec<String> = s
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect();
    let is_stop = |w: &str| DUP_STOP.contains(&w);
    let mut negated = HashSet::new();
    for (i, t) in toks.iter().enumerate() {
        if DUP_FWD_CUES.contains(&t.as_str()) {
            let end = (i + 1 + DUP_FWD_WINDOW).min(toks.len());
            for w in &toks[i + 1..end] {
                if !is_stop(w) && !dup_is_cue(w) {
                    negated.insert(dup_singularize(w).to_string());
                }
            }
        }
        if DUP_BWD_CUES.contains(&t.as_str()) {
            // Nearest content token before the cue: "windows dropped" -> windows.
            for w in toks[..i].iter().rev() {
                if is_stop(w) || dup_is_cue(w) {
                    continue;
                }
                negated.insert(dup_singularize(w).to_string());
                break;
            }
        }
    }
    let content: HashSet<String> = toks
        .iter()
        .filter(|w| !is_stop(w) && !dup_is_cue(w))
        .map(|w| dup_singularize(w).to_string())
        .collect();
    let asserted = content.difference(&negated).cloned().collect();
    (asserted, negated)
}

/// True when the two contents read as a CONTRADICTION rather than a restatement:
/// one asserts a salient token the other negates. Cheap and deterministic — the
/// hint that a high-similarity pair is a supersession (retire the stale one), not
/// a duplicate (merge them). Calibrated in the module tests against a labeled
/// battery.
fn is_supersession(a: &str, b: &str) -> bool {
    let (a_assert, a_neg) = dup_analyze(a);
    let (b_assert, b_neg) = dup_analyze(b);
    a_neg.intersection(&b_assert).next().is_some() || b_neg.intersection(&a_assert).next().is_some()
}

/// `"supersession"` when the pair contradicts (see [`is_supersession`]), else
/// `"duplicate"`.
fn dup_relation(a: &str, b: &str) -> &'static str {
    if is_supersession(a, b) {
        "supersession"
    } else {
        "duplicate"
    }
}

/// Ceiling on how many memories `find_duplicate_memories` compares in one scan.
/// The comparison is O(n^2), so this bounds worst-case work; beyond it the scan
/// reports `truncated: true` rather than doing unbounded work. 2000 memories is
/// ~2M cosine ops over 384-dim vectors — well under a second — while covering
/// any realistic single-project memory store.
const DUP_SCAN_CAP: usize = 2000;

/// Milliseconds per day — the unit `find_stale_memories` converts `older_than_days`
/// and computed ages through.
const MS_PER_DAY: f64 = 86_400_000.0;

/// How many of each category `memory_health` counts before flagging the total a
/// lower bound (`truncated`). Matches the scans' max `limit`.
const HEALTH_COUNT_LIMIT: usize = 1000;

/// How many example rows per category `memory_health` returns — a glance, not
/// the full lists (use the dedicated scan for those).
const HEALTH_SAMPLE: usize = 3;

/// Cosine similarity of two equal-length vectors, or `None` when the lengths
/// differ or either vector has zero magnitude (no defined direction). Matches
/// the engine's `search_vector` scoring so scan results are comparable to
/// write-time `near_duplicates` scores.
fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
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

/// Wall-clock milliseconds since the Unix epoch, for stamping a supersession.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

/// Host display-name convention for evidence rendering: the `name` prop
/// (Entity/Alias), else the first 80 CHARACTERS of `content` (Memory,
/// char-boundary safe, `…` when truncated), else null. The engine
/// deliberately knows nothing about these prop conventions.
fn display_name(n: &topodb::NodeRecord) -> serde_json::Value {
    if let Some(PropValue::Str(name)) = n.props.get("name") {
        return serde_json::Value::String(name.to_string());
    }
    if let Some(PropValue::Str(content)) = n.props.get("content") {
        let mut chars = content.chars();
        let head: String = chars.by_ref().take(80).collect();
        return serde_json::Value::String(if chars.next().is_some() {
            format!("{head}…")
        } else {
            head
        });
    }
    serde_json::Value::Null
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
    /// Embedding subsystem state: model namespace + lifecycle status. Every
    /// write tool that indexes text, and every search/recall tool's vector
    /// leg, consult the embedder directly (see `TopoServer::embedder`'s doc
    /// comment) — this field makes that live status (and
    /// `--embeddings`/`--model-dir`'s effect) observable via `db_info`.
    embeddings: EmbeddingsInfo,
}

/// `db_info`'s embedding-subsystem sub-payload (see [`DbInfo::embeddings`]).
/// `model` is the namespace string reported by `Embedder::model_name`
/// (`--embeddings`'s value, or [`crate::embedder::DEFAULT_MODEL`] when
/// omitted) regardless of whether the model ever reaches `Ready` — a caller
/// diagnosing a `Failed` status still needs to know which model was
/// attempted.
#[derive(Debug, Serialize, JsonSchema)]
struct EmbeddingsInfo {
    model: String,
    status: EmbedderStatus,
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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecentMemoriesParams {
    /// How many memories to return. Default 8.
    #[serde(default = "default_recent_k")]
    #[schemars(range(min = 1, max = 100))]
    k: u32,
    /// Scope to read: `"shared"` or a scope ULID. Defaults to the server's
    /// configured default scope when omitted.
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

fn default_recent_k() -> u32 {
    8
}

#[derive(Debug, Serialize, JsonSchema)]
struct RecentMemoriesResult {
    /// The newest `Memory` nodes in the scope set, most recent first
    /// (id/scope/label/props each).
    memories: Vec<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindDuplicateMemoriesParams {
    /// Cosine floor for calling two memories duplicates (0.0–1.0). Defaults to
    /// the same near-dup floor write-time detection uses (0.80): the default
    /// model scores the same fact in different words ~0.83, unrelated facts well
    /// under 0.5. Raise it for stricter matches, lower to cast a wider (noisier)
    /// net.
    #[serde(default = "default_dup_similarity")]
    #[schemars(range(min = 0.0, max = 1.0))]
    min_similarity: f32,
    /// Cap on the number of pairs returned (most-similar first). Default 100.
    #[serde(default = "default_dup_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u32,
    /// Scope to scan: `"shared"` or a scope ULID. Defaults to the server's
    /// configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Scan across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs.
    /// Takes precedence over `scope`; must not be empty when present.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

fn default_dup_similarity() -> f32 {
    NEAR_DUP_REVIEW
}

fn default_dup_limit() -> u32 {
    100
}

/// One unordered pair of near-duplicate memories found by `find_duplicate_memories`.
#[derive(Debug, Serialize, JsonSchema)]
struct DuplicatePair {
    /// The two memories' ULIDs (ascending, so a pair is reported once).
    ids: [String; 2],
    /// Their contents, index-aligned with `ids`, so the caller can judge "same
    /// fact" from "similar topic" without a follow-up read.
    contents: [String; 2],
    /// Cosine similarity between them (>= `min_similarity`; 1.0 = identical).
    similarity: f32,
    /// Confidence band: `"likely"` (cosine >= 0.80) or `"possible"` (the widened
    /// review band below it, where genuine restatements overlap merely-related
    /// facts — judge before acting).
    band: String,
    /// `"duplicate"` (merge with `consolidate_memories`) or `"supersession"` —
    /// the pair CONTRADICTS (one negates what the other asserts), so it is likely
    /// a fact that replaced the other; retire the stale one with `supersede`
    /// rather than merging. Cosine can't tell these apart (contradictions score
    /// HIGHER than restatements); a negation-cue check does.
    relation: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FindDuplicateMemoriesResult {
    /// Near-duplicate pairs, most-similar first, at most `limit`. Empty when the
    /// embedder is off/not-ready (no semantic signal) or nothing clears the floor.
    pairs: Vec<DuplicatePair>,
    /// How many embedded, non-superseded memories were actually compared.
    scanned: usize,
    /// `true` when the result is NOT exhaustive — either more memories existed
    /// than the scan cap, or more pairs cleared the floor than `limit`. A hint to
    /// narrow scopes or raise `limit`, not an error.
    truncated: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConsolidateMemoriesParams {
    /// ULID of the memory that SURVIVES: it inherits `drop`'s unique
    /// relationships and stays live.
    keep: String,
    /// ULID of the redundant memory to RETIRE: marked superseded and
    /// disconnected. The caller chooses this after judging the two are the same
    /// fact — near-dup similarity is topical, not proof of sameness.
    drop: String,
    /// Scope both memories live in: `"shared"` or a scope ULID. Defaults to the
    /// server's configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
}

/// A relationship `keep` inherited from `drop` during consolidation.
#[derive(Debug, Serialize, JsonSchema)]
struct TransferredEdge {
    /// ULID of the NEW edge created on `keep`.
    edge_id: String,
    /// The edge's target node — the relationship `keep` gained from `drop`.
    to: String,
    /// The edge's (normalized) type, e.g. "about".
    edge_type: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ConsolidateResult {
    /// The surviving memory's ULID (echoes `keep`).
    kept: String,
    /// The retired memory's ULID (echoes `drop`), now marked superseded.
    dropped: String,
    /// Relationships `drop` had that `keep` did not — recreated on `keep` so no
    /// graph knowledge is lost. Empty when `keep` already had every link `drop`
    /// did (the common true-duplicate case).
    transferred_edges: Vec<TransferredEdge>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindOrphanMemoriesParams {
    /// Cap on the number of orphans returned (oldest first). Default 100.
    #[serde(default = "default_orphan_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u32,
    /// Scope to scan: `"shared"` or a scope ULID. Defaults to the server's
    /// configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Scan across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs.
    /// Takes precedence over `scope`; must not be empty when present.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

fn default_orphan_limit() -> u32 {
    100
}

/// A memory connected to nothing — a live Memory with no open outgoing edges.
#[derive(Debug, Serialize, JsonSchema)]
struct OrphanMemory {
    /// The orphan memory's ULID.
    id: String,
    /// Its content, so the caller can decide whether to link or drop it without
    /// a follow-up read.
    content: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FindOrphanMemoriesResult {
    /// Memories linked to nothing, oldest first, at most `limit`. Empty when
    /// every stored memory is connected.
    orphans: Vec<OrphanMemory>,
    /// How many live (non-superseded) memories were examined.
    scanned: usize,
    /// `true` when more orphans exist than `limit` returned. A hint to raise
    /// `limit` or narrow scopes, not an error.
    truncated: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindStaleMemoriesParams {
    /// Minimum age in days of a memory's LAST activity — the later of its
    /// creation and its most recent recall — for it to count as stale. Default
    /// 30. A memory created or recalled more recently than this is fresh and
    /// excluded, so a brand-new memory is never stale.
    #[serde(default = "default_stale_days")]
    older_than_days: f64,
    /// Cap on the number of stale memories returned (stalest first). Default 100.
    #[serde(default = "default_stale_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u32,
    /// Scope to scan: `"shared"` or a scope ULID. Defaults to the server's
    /// configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Scan across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs.
    /// Takes precedence over `scope`; must not be empty when present.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

fn default_stale_days() -> f64 {
    30.0
}

fn default_stale_limit() -> u32 {
    100
}

/// A memory that has gone cold — no activity within the requested window.
#[derive(Debug, Serialize, JsonSchema)]
struct StaleMemory {
    /// The stale memory's ULID.
    id: String,
    /// Its content, so the caller can decide to refresh, re-link, or drop it.
    content: String,
    /// Times this memory has been returned by a scoped read. 0 = never recalled.
    access_count: u64,
    /// Wall-clock ms of the most recent recall; omitted (null) when never
    /// recalled — staleness is then measured from creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_accessed_at: Option<i64>,
    /// Days since the memory's last activity (creation or recall).
    age_days: f64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct FindStaleMemoriesResult {
    /// Cold memories, stalest first, at most `limit`. Empty when everything is
    /// fresher than `older_than_days`.
    stale: Vec<StaleMemory>,
    /// How many live (non-superseded) memories were examined.
    scanned: usize,
    /// `true` when more stale memories exist than `limit` returned. A hint to
    /// raise `limit` or narrow scopes, not an error.
    truncated: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MemoryHealthParams {
    /// Staleness threshold in days, passed through to the stale check (a memory
    /// is stale when the later of its creation and last recall is older than
    /// this). Default 30.
    #[serde(default = "default_stale_days")]
    stale_older_than_days: f64,
    /// Scope to assess: `"shared"` or a scope ULID. Defaults to the server's
    /// configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Assess across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs.
    /// Takes precedence over `scope`; must not be empty when present.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct MemoryHealthResult {
    /// Live (non-superseded) memories in the scope set.
    total_memories: usize,
    /// Whether the embedder is Ready. When `false`, `duplicate_pairs` is 0
    /// because near-duplicate detection needs embeddings — NOT because there are
    /// none.
    embeddings_enabled: bool,
    /// Near-duplicate pairs that look like the SAME fact (cosine >= 0.80,
    /// non-contradicting) — merge with `consolidate_memories`. 0 when embeddings
    /// are off.
    duplicate_pairs: usize,
    /// High-similarity pairs that CONTRADICT each other (one negates what the
    /// other asserts) — likely a fact that replaced an older one; retire the
    /// stale side with `supersede`, don't merge. Split out from `duplicate_pairs`
    /// because cosine scores contradictions even higher than restatements.
    supersession_pairs: usize,
    /// Memories linked to nothing (no open outgoing edges).
    orphan_count: usize,
    /// Memories with no activity (creation or recall) within `stale_older_than_days`.
    stale_count: usize,
    /// `true` if any category is non-zero — the one-glance "does my memory need
    /// tidying?" signal.
    needs_attention: bool,
    /// Up to a few most-similar duplicate pairs, for orientation. Use
    /// `find_duplicate_memories` for the full list.
    sample_duplicates: Vec<DuplicatePair>,
    /// Up to a few orphans, oldest first. Use `find_orphan_memories` for all.
    sample_orphans: Vec<OrphanMemory>,
    /// Up to a few stalest memories. Use `find_stale_memories` for all.
    sample_stale: Vec<StaleMemory>,
    /// `true` if any underlying scan hit its cap, so the counts are lower bounds.
    truncated: bool,
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

fn default_weight_one() -> f32 {
    1.0
}

fn default_weight_half() -> f32 {
    0.5
}

fn default_labels() -> Vec<String> {
    vec!["Memory".to_string(), "Entity".to_string()]
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
    /// Typo/prefix recovery for query terms that match nothing (default
    /// true): a missing term expands to its closest vocabulary neighbors
    /// (prefix or small edit distance) at a score discount, so exact matches
    /// always dominate. Set false for strict term matching.
    #[serde(default = "default_true")]
    fuzzy: bool,
    /// Pull 1-hop graph neighbors of top hits into the results (linked
    /// context). Default true; set false for lexical/semantic-only.
    #[serde(default = "default_true")]
    graph_boost: bool,
    /// Result label allowlist. Defaults to ["Memory","Entity"] — memories
    /// plus the named entities they link to; Alias/Synonym plumbing nodes
    /// never surface by default. Override to widen (e.g. add "Episode")
    /// or narrow (["Memory"]). Must not be empty when present. A narrowing
    /// filter is applied post-fusion, so a filtered search may return
    /// fewer than `k` results.
    #[serde(default = "default_labels")]
    #[schemars(length(min = 1))]
    labels: Vec<String>,
    /// RRF weight of the BM25 text leg (0-10, default 1).
    #[serde(default = "default_weight_one")]
    #[schemars(range(min = 0.0, max = 10.0))]
    text_weight: f32,
    /// RRF weight of the vector leg (0-10, default 1). Only meaningful
    /// when embeddings are ready.
    #[serde(default = "default_weight_one")]
    #[schemars(range(min = 0.0, max = 10.0))]
    vector_weight: f32,
    /// RRF weight of the 1-hop graph leg (0-10, default 0.5); applies when
    /// graph_boost is on.
    #[serde(default = "default_weight_half")]
    #[schemars(range(min = 0.0, max = 10.0))]
    graph_weight: f32,
    /// How much access history lifts ranking (0-1, default 0 = off):
    /// frequently-recalled memories rank higher at equal relevance,
    /// log-damped. Neutral on a node never recalled.
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    access_weight: f32,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SearchHit {
    /// The matched node (id/scope/label/props).
    node: Value,
    /// Relevance score, higher is more relevant. For search_memories this is the fused
    /// hybrid (RRF) rank score — small magnitudes (~0.01–0.05), only comparable within a
    /// single response, NOT a BM25 or similarity value to threshold on. For search_vectors
    /// it is cosine similarity.
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
    /// ULID of the node to start the traversal from. Provide this OR
    /// `seed_ids`; if both are given, `seed_ids` wins.
    #[serde(default)]
    seed_id: Option<String>,
    /// Start the traversal from SEVERAL nodes at once — e.g. every hit from a
    /// `search_memories` call — to explore the graph around all of them in a
    /// single traverse instead of one call per anchor. Must not be empty when
    /// present. Takes precedence over `seed_id`.
    #[serde(default)]
    #[schemars(length(min = 1))]
    seed_ids: Option<Vec<String>>,
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
    /// `{"nodes": [...], "edges": [...]}` reached from the seed(s).
    subgraph: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SuggestLinksParams {
    /// Node to suggest missing links for (ULID).
    node_id: String,
    /// How many suggestions. Default 5.
    #[serde(default = "default_suggest_k")]
    #[schemars(range(min = 1, max = 50))]
    k: u32,
    /// Semantic-leg floor: suggestions whose cosine (against the target's
    /// own embedding) falls below this are dropped from the semantic
    /// signal. Model-dependent — omit unless you know your embedder's
    /// similarity distribution. No default.
    #[serde(default)]
    #[schemars(range(min = -1.0, max = 1.0))]
    min_similarity: Option<f32>,
    /// Scope to read: `"shared"` or a scope ULID. Defaults to the server's
    /// configured default scope when omitted.
    #[serde(default)]
    scope: Option<String>,
    /// Read across SEVERAL scopes at once (takes precedence over `scope`).
    /// Must not be empty when present.
    #[serde(default)]
    #[schemars(length(min = 1))]
    scopes: Option<Vec<String>>,
}

fn default_suggest_k() -> u32 {
    5
}

#[derive(Debug, Serialize, JsonSchema)]
struct SuggestLinksResult {
    /// Suggested-but-nonexistent edges, best first: `{node, score,
    /// similarity, common_neighbors, structural, semantic}` each.
    /// `similarity` is the raw cosine when the suggestion came through the
    /// semantic leg (`null` = found structurally); `common_neighbors`
    /// entries are `{id, label, name}` shared 1-hop nodes — the evidence.
    suggestions: Vec<Value>,
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
struct RememberParams {
    /// The memory's full-text-searchable body (embedded for semantic recall
    /// when embeddings are on) — same semantics as `create_memory.content`.
    content: String,
    /// Names of the entities this fact concerns. Each is resolved
    /// find-or-create with `create_entity`'s exact semantics (case- and
    /// whitespace-insensitive across the read scopes, the write scope, and
    /// shared; alias-aware; never duplicates). At least one is required —
    /// `remember` is the linked-fact verb; use `create_memory` for a
    /// deliberately unlinked note. Repeated names within one call collapse
    /// to a single entity and a single link.
    #[schemars(length(min = 1))]
    entities: Vec<String>,
    /// One edge type applied to every memory→entity link. Defaults to
    /// `"about"`. Normalized like `link` normalizes it (`Works At` ==
    /// `works_at`).
    #[serde(default)]
    edge_type: Option<String>,
    /// Structured metadata merged into the MEMORY node's props
    /// (string/number/bool values). Must not include a `content` key — that
    /// key is set from the `content` param above; a collision is rejected
    /// rather than silently overwritten.
    #[serde(default)]
    #[schemars(with = "Option<PropsSchema>")]
    props: Option<Value>,
    /// Single write scope for EVERYTHING this call creates — the memory,
    /// any new entity nodes, and all edges: `"shared"` or a scope ULID.
    /// Defaults to the server's configured default scope. When the fact
    /// concerns shared-scope entities and should be visible outside this
    /// project, pass `"shared"` — a project-scoped edge to a shared entity
    /// is invisible to other projects.
    #[serde(default)]
    scope: Option<String>,
    /// Memory ULIDs this new fact REPLACES. Each is marked superseded (dated,
    /// not deleted) and unlinked from its entities, so it stops surfacing in
    /// search_memories/traverse while remaining visible to an `as_of` read
    /// before now. Use when a fact changes ("uses JWT" → "uses PASETO"): store
    /// the new memory and pass the old one's id here. The ids must be memories
    /// in this write scope. Empty/omitted supersedes nothing.
    #[serde(default)]
    #[schemars(length(min = 1))]
    supersedes: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RememberedEntity {
    /// The name as given in the call (first spelling wins when repeats
    /// collapse).
    name: String,
    /// ULID of the entity this name resolved to (or the new node).
    id: String,
    /// `false` means the name resolved to an existing entity — no new node.
    created: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct RememberResult {
    /// ULID of the memory node — newly created, or the existing memory if this
    /// exact content was already stored in the write scope.
    memory_id: String,
    /// One row per distinct entity, in input order.
    entities: Vec<RememberedEntity>,
    /// ULIDs of the memory→entity edges, index-aligned with `entities`. On a
    /// dedup hit, an entity already linked to the existing memory reports its
    /// existing edge id (no duplicate edge is created).
    edge_ids: Vec<String>,
    /// True if this exact content already existed: the existing memory was
    /// reused and only entities not already linked to it were newly linked.
    deduplicated: bool,
    /// ULIDs actually marked superseded by this call (a subset of the
    /// requested `supersedes` — an already-superseded id is not re-marked).
    superseded: Vec<String>,
    /// Existing memories semantically close to the one just stored (advisory —
    /// nothing was merged). Non-empty only when embeddings are on. Empty on a
    /// dedup hit. See `NearDuplicate`.
    near_duplicates: Vec<NearDuplicate>,
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

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddAliasParams {
    /// ULID of the canonical Entity this alias names.
    entity_id: String,
    /// The alternate name. Matched case/whitespace-insensitively everywhere
    /// entity names are.
    alias: String,
    /// Scope for the alias node + edge. Defaults to the canonical entity's
    /// own scope (NOT the server default — an alias belongs with its entity).
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddSynonymParams {
    /// Query word this expansion applies to (normalized on store).
    term: String,
    /// The equivalent word/phrase searches should also try.
    expansion: String,
    /// Also register the reverse direction (expansion -> term). Default true.
    #[serde(default = "default_true")]
    bidirectional: bool,
    /// Scope for the synonym node(s); defaults to the server write scope.
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct AddSynonymResult {
    /// Synonym node id(s) — one per direction written or reused.
    ids: Vec<String>,
    /// False when every requested direction already existed.
    created: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct CreateResult {
    /// ULID of the node — the newly created one, or the existing memory if
    /// this exact content was already stored in the write scope.
    id: String,
    /// True if an identical memory already existed in the write scope and was
    /// returned instead of creating a duplicate.
    deduplicated: bool,
    /// Existing memories semantically close to the one just stored (advisory —
    /// nothing was merged). Non-empty only when embeddings are on; consider
    /// whether a hit is actually the same fact and, if so, `supersedes` or
    /// `remove_node` the redundant one. Empty on a dedup hit.
    near_duplicates: Vec<NearDuplicate>,
}

/// A semantically-similar existing memory surfaced to the caller. Advisory:
/// similarity is not identity — a high score can still be two different facts,
/// so this is a signal for the caller to judge, never an automatic merge.
#[derive(Debug, Serialize, JsonSchema)]
struct NearDuplicate {
    /// ULID of the similar existing memory.
    id: String,
    /// Its content, so the caller can tell "same fact" from "similar topic".
    content: String,
    /// Cosine similarity to the memory just stored (1.0 = identical direction).
    similarity: f32,
    /// Confidence band: `"likely"` (cosine >= 0.80) or `"possible"` (review band).
    band: String,
    /// `"duplicate"` or `"supersession"` — if the existing memory CONTRADICTS the
    /// one being stored (negates what it asserts), this is the fact being
    /// replaced; `supersede` it rather than treating it as a duplicate.
    relation: String,
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
        description = "Report the open database's path, current op-log sequence number, the default WRITE scope applied to a create/link call that omits scope, the default READ scope set applied to a read call that omits both scope/scopes, and the embedding subsystem's model name + lifecycle status (off/downloading/ready/failed). Call this first to confirm the server is wired to the expected database and read set, and to obtain current_seq as the anchor for get_changes. NOTE: the default read set can be WIDER than the default write scope (e.g. --read-scopes project,shared with --scope project) — passing default_scope as a read call's own `scope` NARROWS the read to that one scope, which can be stricter than staying on the defaults."
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
            embeddings: EmbeddingsInfo {
                model: self.embedder.model_name(),
                status: self.embedder.status(),
            },
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
            self.db
                .nodes_by_prop(&scope_set, &p.label, &p.prop, &value)
                .map_err(|e| match e {
                    TopoError::Rejected(_) => ErrorData::invalid_params(e.to_string(), None),
                    other => ErrorData::internal_error(other.to_string(), None),
                })?
        } else if p.label == ENTITY_LABEL && p.prop == ENTITY_NAME_PROP {
            // Alias-aware: an alias name resolves to its canonical entity
            // (Task 8), same as create_entity's dedup lookup. Only this
            // specific (label, prop) pair carries alias semantics — any
            // other equality-indexed lookup keeps the plain normalized match.
            let name = match &value {
                PropValue::Str(s) => s.clone(),
                other => {
                    return Err(ErrorData::invalid_params(
                        format!("(Entity, name) matches string values only, got {other:?}"),
                        None,
                    ))
                }
            };
            self.resolve_entities_by_name(&scope_set, &name)
                .map_err(|e| match e {
                    TopoError::Rejected(_) => ErrorData::invalid_params(e.to_string(), None),
                    other => ErrorData::internal_error(other.to_string(), None),
                })?
        } else {
            self.db
                .nodes_by_prop_normalized(&scope_set, &p.label, &p.prop, &value)
                .map_err(|e| match e {
                    TopoError::Rejected(_) => ErrorData::invalid_params(e.to_string(), None),
                    other => ErrorData::internal_error(other.to_string(), None),
                })?
        };
        let nodes = hits
            .iter()
            .map(convert::node_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(FindByPropResult { nodes }))
    }

    #[tool(
        description = "The newest memories in the read scopes, most recent first. For orientation ('what was I doing?', session-start context), not search — use search_memories when you know what you're looking for. k defaults to 8 (max 100)."
    )]
    fn recent_memories(
        &self,
        Parameters(p): Parameters<RecentMemoriesParams>,
    ) -> Result<Json<RecentMemoriesResult>, ErrorData> {
        if !(1..=100).contains(&p.k) {
            return Err(ErrorData::invalid_params(
                format!("k must be between 1 and 100, got {}", p.k),
                None,
            ));
        }
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        // Near-O(k) via LABEL_INDEX reverse-bounded scans (F9-11 Task 8),
        // not a full label scan + sort — `nodes_by_label_newest` already
        // returns newest-first (ULIDs sort by mint time: descending id =
        // newest first) and k-bounded.
        let nodes = self
            .db
            .nodes_by_label_newest(&scope_set, MEMORY_LABEL, p.k as usize);
        let memories = nodes
            .iter()
            .map(convert::node_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(RecentMemoriesResult { memories }))
    }

    #[tool(
        description = "Maintenance scan: find pairs of ALREADY-STORED memories that are semantically near-duplicates (cosine >= min_similarity, default 0.70), most-similar first. Read-only and advisory. Each pair carries a `band` — `likely` (cosine >= 0.80) or `possible` (0.70-0.80, a wider net where genuine restatements overlap merely-related facts, so judge the contents before acting) — and a `relation`: `duplicate` (same fact reworded -> merge with `consolidate_memories`) or `supersession` (the two CONTRADICT — one negates what the other asserts, so it's a fact that replaced the older one -> retire the stale side with `supersede`, don't merge). The relation matters because cosine scores contradictions HIGHER than restatements, so similarity alone can't tell them apart. Empty when the embedder is off/not-ready. Capped at `limit` (and the scan at an internal cap); `truncated=true` means not exhaustive."
    )]
    fn find_duplicate_memories(
        &self,
        Parameters(p): Parameters<FindDuplicateMemoriesParams>,
    ) -> Result<Json<FindDuplicateMemoriesResult>, ErrorData> {
        if !(0.0..=1.0).contains(&p.min_similarity) {
            return Err(ErrorData::invalid_params(
                format!(
                    "min_similarity must be between 0.0 and 1.0, got {}",
                    p.min_similarity
                ),
                None,
            ));
        }
        if !(1..=1000).contains(&p.limit) {
            return Err(ErrorData::invalid_params(
                format!("limit must be between 1 and 1000, got {}", p.limit),
                None,
            ));
        }
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        Ok(Json(self.duplicate_scan(
            &scope_set,
            p.min_similarity,
            p.limit as usize,
        )))
    }

    /// Core of [`find_duplicate_memories`] — no param validation or scope
    /// resolution (callers do those), so `memory_health` can reuse the exact
    /// same detection instead of re-deriving it.
    fn duplicate_scan(
        &self,
        scope_set: &ScopeSet,
        min_similarity: f32,
        limit: usize,
    ) -> FindDuplicateMemoriesResult {
        let model = self.embedder.model_name();

        // Candidates: Memory nodes carrying a same-model embedding that are NOT
        // already retired. Superseded memories are excluded — they were retired
        // on purpose, so re-flagging them as duplicates is noise. Embeddings-off
        // stores have no vectors, so this is empty (no semantic signal).
        let mut candidates: Vec<(String, String, Vec<f32>)> = self
            .db
            .nodes_by_label_unbumped(scope_set, MEMORY_LABEL)
            .into_iter()
            .filter(|n| !n.props.contains_key(convert::MEMORY_SUPERSEDED_AT_PROP))
            .filter_map(|n| {
                let (m, v) = n.embedding?;
                if m != model {
                    return None;
                }
                let content = match n.props.get(MEMORY_CONTENT_PROP) {
                    Some(PropValue::Str(c)) => c.clone(),
                    _ => String::new(),
                };
                Some((n.id.to_string(), content, v))
            })
            .collect();

        // Bound the O(n^2) comparison. Beyond the cap we compare a prefix and
        // flag the result non-exhaustive rather than doing unbounded work — the
        // candidates come from `nodes_by_label` in id (mint-time) order, so the
        // prefix is the oldest memories, the ones most likely to have accreted
        // duplicates.
        let mut truncated = candidates.len() > DUP_SCAN_CAP;
        candidates.truncate(DUP_SCAN_CAP);
        let scanned = candidates.len();

        // Complete pairwise cosine over the bounded set (not a k-capped index
        // probe), so every pair above the floor is found, not just the top few
        // per memory.
        let mut pairs: Vec<DuplicatePair> = Vec::new();
        for i in 0..candidates.len() {
            for j in (i + 1)..candidates.len() {
                if let Some(sim) = cosine(&candidates[i].2, &candidates[j].2) {
                    if sim >= min_similarity {
                        let (a, b) = (&candidates[i], &candidates[j]);
                        // Canonical (ascending-id) order so a pair is reported once.
                        let (lo, hi) = if a.0 <= b.0 { (a, b) } else { (b, a) };
                        pairs.push(DuplicatePair {
                            ids: [lo.0.clone(), hi.0.clone()],
                            similarity: sim,
                            band: dup_band(sim).to_string(),
                            relation: dup_relation(&lo.1, &hi.1).to_string(),
                            contents: [lo.1.clone(), hi.1.clone()],
                        });
                    }
                }
            }
        }
        // Most-similar first; NaN can't occur (finite vectors, non-zero norms
        // filtered by `cosine`), so total_cmp is a safe total order.
        pairs.sort_by(|x, y| y.similarity.total_cmp(&x.similarity));
        if pairs.len() > limit {
            truncated = true;
            pairs.truncate(limit);
        }
        FindDuplicateMemoriesResult {
            pairs,
            scanned,
            truncated,
        }
    }

    #[tool(
        description = "Consolidate a near-duplicate PAIR into one memory: keep one, retire the other. YOU pick which survives (keep) and which is retired (drop) after judging they are the same fact — never let the tool infer it, because near-dup similarity is topical, not factual (a contradicting correction about the same subsystem scores high too). keep inherits drop's unique relationships (so no graph knowledge is lost) and drop is superseded — marked and disconnected — atomically. Pair this with find_duplicate_memories: scan for pairs, judge them, consolidate the true duplicates. Errors unless both are live (non-superseded) Memory nodes in the write scope and keep != drop."
    )]
    fn consolidate_memories(
        &self,
        Parameters(p): Parameters<ConsolidateMemoriesParams>,
    ) -> Result<Json<ConsolidateResult>, ErrorData> {
        let keep = parse_node_id(&p.keep)?;
        let drop = parse_node_id(&p.drop)?;
        if keep == drop {
            return Err(ErrorData::invalid_params(
                "keep and drop must be different memories".to_string(),
                None,
            ));
        }
        let scope = self.resolve_scope(p.scope.as_deref())?;
        let write_set = convert::scope_to_scope_set(scope);

        // Both must be live Memory nodes in the write scope. supersede_ops
        // re-checks drop, but validate both up front for a clear error before
        // building any ops — and to reject an already-superseded node rather than
        // silently no-op it.
        let require_live_memory = |id: NodeId, raw: &str, role: &str| -> Result<(), ErrorData> {
            let node = self.db.node(&write_set, id).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("{role} id {raw} is not a node in the write scope"),
                    None,
                )
            })?;
            if node.label != MEMORY_LABEL {
                return Err(ErrorData::invalid_params(
                    format!("{role} id {raw} is a {}, not a Memory", node.label),
                    None,
                ));
            }
            if node.props.contains_key(convert::MEMORY_SUPERSEDED_AT_PROP) {
                return Err(ErrorData::invalid_params(
                    format!("{role} id {raw} is already superseded"),
                    None,
                ));
            }
            Ok(())
        };
        require_live_memory(keep, &p.keep, "keep")?;
        require_live_memory(drop, &p.drop, "drop")?;

        // Relationships keep already has, keyed by (target, type), so inheritance
        // never stacks a duplicate edge.
        let mut have: std::collections::BTreeSet<(NodeId, String)> = self
            .db
            .edges_from(&write_set, keep, None, None, true)
            .map_err(classify_topo_error)?
            .into_iter()
            .map(|e| (e.to, e.ty.to_string()))
            .collect();

        let mut ops: Vec<Op> = Vec::new();
        let mut transferred: Vec<TransferredEdge> = Vec::new();
        for e in self
            .db
            .edges_from(&write_set, drop, None, None, true)
            .map_err(classify_topo_error)?
        {
            // Never point keep at itself or at the node being retired.
            if e.to == keep || e.to == drop {
                continue;
            }
            // insert() returns true only when keep lacked this (target, type).
            if have.insert((e.to, e.ty.to_string())) {
                let id = EdgeId::new();
                transferred.push(TransferredEdge {
                    edge_id: id.to_string(),
                    to: e.to.to_string(),
                    edge_type: e.ty.to_string(),
                });
                ops.push(Op::CreateEdge {
                    id,
                    scope,
                    ty: e.ty,
                    from: keep,
                    to: e.to,
                    props: e.props,
                    valid_from: None,
                });
            }
        }

        // Retire drop in the SAME batch, so keep's inheritance and drop's
        // retirement commit together — keep can never absorb the edges and then
        // fail to retire the duplicate.
        let (sup_ops, _marked) = self.supersede_ops(scope, std::slice::from_ref(&p.drop))?;
        ops.extend(sup_ops);
        self.submit_write(ops)?;

        Ok(Json(ConsolidateResult {
            kept: keep.to_string(),
            dropped: drop.to_string(),
            transferred_edges: transferred,
        }))
    }

    #[tool(
        description = "Maintenance scan: find memories that are stored but connected to NOTHING — a live memory with no open outgoing edges, so it joined no entity and is reachable only by text/vector search, never by traversal. Usually a bare create_memory that was never linked, or a memory whose only link was later closed. Read-only and advisory: link the orphan to its entities (link/remember) or drop it. Superseded memories are excluded — their edges close on retirement, so they are retired, not orphaned. Oldest first, at most `limit`; `truncated=true` means more orphans exist than were returned."
    )]
    fn find_orphan_memories(
        &self,
        Parameters(p): Parameters<FindOrphanMemoriesParams>,
    ) -> Result<Json<FindOrphanMemoriesResult>, ErrorData> {
        if !(1..=1000).contains(&p.limit) {
            return Err(ErrorData::invalid_params(
                format!("limit must be between 1 and 1000, got {}", p.limit),
                None,
            ));
        }
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        Ok(Json(self.orphan_scan(&scope_set, p.limit as usize)?))
    }

    /// Core of [`find_orphan_memories`] — no validation/scope resolution, so
    /// `memory_health` reuses the identical orphan definition.
    fn orphan_scan(
        &self,
        scope_set: &ScopeSet,
        limit: usize,
    ) -> Result<FindOrphanMemoriesResult, ErrorData> {
        let mut orphans: Vec<OrphanMemory> = Vec::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        // nodes_by_label yields oldest-first (ascending id), so orphans come out
        // oldest-first without a sort. Each memory needs one indexed out-edge
        // lookup — O(n), not O(n^2), so no scan cap is needed; only the returned
        // list is bounded.
        for n in self.db.nodes_by_label_unbumped(scope_set, MEMORY_LABEL) {
            // Retired memories have closed edges by design — not orphans.
            if n.props.contains_key(convert::MEMORY_SUPERSEDED_AT_PROP) {
                continue;
            }
            scanned += 1;
            let open = self
                .db
                .edges_from(scope_set, n.id, None, None, true)
                .map_err(classify_topo_error)?;
            if !open.is_empty() {
                continue;
            }
            if orphans.len() >= limit {
                // Keep counting `scanned` for an honest total, but stop growing
                // the list and flag the truncation.
                truncated = true;
                continue;
            }
            let content = match n.props.get(MEMORY_CONTENT_PROP) {
                Some(PropValue::Str(c)) => c.clone(),
                _ => String::new(),
            };
            orphans.push(OrphanMemory {
                id: n.id.to_string(),
                content,
            });
        }
        Ok(FindOrphanMemoriesResult {
            orphans,
            scanned,
            truncated,
        })
    }

    #[tool(
        description = "Maintenance scan: find memories that have gone COLD — not created or recalled within older_than_days (default 30), stalest first. 'Activity' is the later of a memory's creation and its most recent recall (last_accessed_at), so a brand-new memory is never stale and a frequently-recalled one stays fresh; a fact stored long ago and never looked at since is what surfaces. Read-only and advisory: review, then refresh (re-link), keep, or drop. The scan itself does NOT count as a recall — it inspects the recency signal without bumping it. Superseded memories are excluded. Each row carries access_count, last_accessed_at (null if never recalled), and age_days. Stalest first, at most `limit`; truncated=true means more exist."
    )]
    fn find_stale_memories(
        &self,
        Parameters(p): Parameters<FindStaleMemoriesParams>,
    ) -> Result<Json<FindStaleMemoriesResult>, ErrorData> {
        if !(1..=1000).contains(&p.limit) {
            return Err(ErrorData::invalid_params(
                format!("limit must be between 1 and 1000, got {}", p.limit),
                None,
            ));
        }
        if !p.older_than_days.is_finite() || p.older_than_days < 0.0 {
            return Err(ErrorData::invalid_params(
                format!(
                    "older_than_days must be a finite number >= 0.0, got {}",
                    p.older_than_days
                ),
                None,
            ));
        }
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        Ok(Json(self.stale_scan(
            &scope_set,
            p.older_than_days,
            p.limit as usize,
        )?))
    }

    /// Core of [`find_stale_memories`] — no validation/scope resolution, so
    /// `memory_health` reuses the identical staleness definition.
    fn stale_scan(
        &self,
        scope_set: &ScopeSet,
        older_than_days: f64,
        limit: usize,
    ) -> Result<FindStaleMemoriesResult, ErrorData> {
        let now = now_ms();
        let threshold_ms = (older_than_days * MS_PER_DAY) as i64;

        // (effective_last_activity_ms, row) so we can sort stalest-first after.
        let mut candidates: Vec<(i64, StaleMemory)> = Vec::new();
        let mut scanned = 0usize;
        // Unbumped: this is housekeeping, not a recall. Bumping would reset the
        // very last_accessed_at we read, making the whole store look fresh on the
        // next scan.
        for n in self.db.nodes_by_label_unbumped(scope_set, MEMORY_LABEL) {
            if n.props.contains_key(convert::MEMORY_SUPERSEDED_AT_PROP) {
                continue;
            }
            scanned += 1;
            let stats = self
                .db
                .access_stats(scope_set, n.id)
                .map_err(classify_topo_error)?
                .unwrap_or_default();
            // Activity = later of creation (ULID mint) and last recall. A memory
            // never recalled (last_accessed_at == 0) falls back to its mint time.
            let effective = (n.id.timestamp_ms() as i64).max(stats.last_accessed_at);
            let age_ms = now - effective;
            if age_ms < threshold_ms {
                continue;
            }
            let content = match n.props.get(MEMORY_CONTENT_PROP) {
                Some(PropValue::Str(c)) => c.clone(),
                _ => String::new(),
            };
            candidates.push((
                effective,
                StaleMemory {
                    id: n.id.to_string(),
                    content,
                    access_count: stats.access_count,
                    last_accessed_at: (stats.last_accessed_at != 0)
                        .then_some(stats.last_accessed_at),
                    age_days: age_ms as f64 / MS_PER_DAY,
                },
            ));
        }
        // Stalest first = oldest activity first (ascending effective timestamp).
        // Stable sort keeps id (mint) order among equal-activity memories.
        candidates.sort_by_key(|(effective, _)| *effective);
        let truncated = candidates.len() > limit;
        let stale = candidates.into_iter().take(limit).map(|(_, m)| m).collect();
        Ok(FindStaleMemoriesResult {
            stale,
            scanned,
            truncated,
        })
    }

    #[tool(
        description = "Memory health check: one call that runs the hygiene scans (near-duplicates, orphans, stale) over the scope and returns a consolidated summary — counts, a `needs_attention` flag, and a few sample rows. The 'what needs tidying in my memory?' orientation read for session start, so an agent doesn't have to remember the separate maintenance tools. Read-only and advisory; drill into any non-zero category with find_duplicate_memories / find_orphan_memories / find_stale_memories, then act. Near-dup pairs (cosine >= 0.80) are split by relation: `duplicate_pairs` (same fact -> consolidate) vs `supersession_pairs` (contradicting facts -> supersede the stale one). `duplicate_pairs`/`supersession_pairs` are 0 when the embedder is off — check `embeddings_enabled` to tell 'none' from 'couldn't check'. Counts cap at an internal limit; truncated=true means lower bounds."
    )]
    fn memory_health(
        &self,
        Parameters(p): Parameters<MemoryHealthParams>,
    ) -> Result<Json<MemoryHealthResult>, ErrorData> {
        if !p.stale_older_than_days.is_finite() || p.stale_older_than_days < 0.0 {
            return Err(ErrorData::invalid_params(
                format!(
                    "stale_older_than_days must be a finite number >= 0.0, got {}",
                    p.stale_older_than_days
                ),
                None,
            ));
        }
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;

        // Reuse the exact scan cores so the health summary can never disagree
        // with the dedicated tools about what a duplicate/orphan/stale memory is.
        let dups = self.duplicate_scan(&scope_set, NEAR_DUP_THRESHOLD, HEALTH_COUNT_LIMIT);
        let orphans = self.orphan_scan(&scope_set, HEALTH_COUNT_LIMIT)?;
        let stale = self.stale_scan(&scope_set, p.stale_older_than_days, HEALTH_COUNT_LIMIT)?;

        // Both orphan and stale scans count EVERY live memory in `scanned` (the
        // list cap bounds only the returned rows), so either gives the true total.
        let total_memories = stale.scanned;
        let embeddings_enabled = matches!(self.embedder.status(), EmbedderStatus::Ready);
        // Split the near-dup pairs by relation: same-fact restatements are
        // duplicates (merge), contradictions are supersessions (retire the stale
        // side). Sample only the true duplicates.
        let (mut sample_duplicates, supersessions): (Vec<DuplicatePair>, Vec<DuplicatePair>) = dups
            .pairs
            .into_iter()
            .partition(|p| p.relation == "duplicate");
        let duplicate_pairs = sample_duplicates.len();
        let supersession_pairs = supersessions.len();
        let orphan_count = orphans.orphans.len();
        let stale_count = stale.stale.len();
        let needs_attention =
            duplicate_pairs > 0 || supersession_pairs > 0 || orphan_count > 0 || stale_count > 0;
        let truncated = dups.truncated || orphans.truncated || stale.truncated;

        sample_duplicates.truncate(HEALTH_SAMPLE);
        let mut sample_orphans = orphans.orphans;
        sample_orphans.truncate(HEALTH_SAMPLE);
        let mut sample_stale = stale.stale;
        sample_stale.truncate(HEALTH_SAMPLE);

        Ok(Json(MemoryHealthResult {
            total_memories,
            embeddings_enabled,
            duplicate_pairs,
            supersession_pairs,
            orphan_count,
            stale_count,
            needs_attention,
            sample_duplicates,
            sample_orphans,
            sample_stale,
            truncated,
        }))
    }

    #[tool(
        description = "Full-text BM25 search over indexed text (memory content AND entity names), recency-weighted: at equal relevance, fresher memories rank above stale ones (tune with recency_weight, 0 = pure BM25). Terms are stemmed ('databases' matches 'database', 'running' matches 'run') and camelCase identifiers split; a term that matches nothing falls back to close prefix/typo neighbors at a score discount. Learned synonyms (add_synonym) expand queries automatically, and 1-hop linked context is pulled in (graph_boost, default true). If a query returns nothing useful, retry with different words, raise k, or widen scopes before concluding nothing is stored. Then traverse from the best hit to gather its linked context. Results are filtered to Memory and Entity nodes by default (labels param overrides); leg weights (text_weight/vector_weight/graph_weight) and an access-history boost (access_weight, default off) tune ranking."
    )]
    fn search_memories(
        &self,
        Parameters(p): Parameters<SearchMemoriesParams>,
    ) -> Result<Json<SearchMemoriesResult>, ErrorData> {
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        // Resolve synonyms per query word. Lookup key is the ANALYZED
        // (stemmed) form via topodb::analyze, matching how add_synonym
        // stores terms — so "logins" finds a synonym stored for "login".
        // Degrade silently when the spec has no Synonym index. Spec cap:
        // at most 4 expansions per term, lexicographically smallest first
        // (deterministic).
        let mut expansions: Vec<(String, Vec<String>)> = Vec::new();
        // Dedup query words by their analyzed key: a duplicate/
        // morphologically-equal word ("auth auth", or "logins" after
        // "login") would otherwise look up and push the SAME synonym set
        // twice, and `search_text_expanded`'s per-scope discount only
        // corroborates each distinct token once anyway — a second identical
        // expansion entry is pure waste, not extra signal.
        let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for word in p.query.split_whitespace() {
            let Some(key) = topodb::analyze(word).into_iter().next() else {
                continue;
            };
            if !seen_keys.insert(key.clone()) {
                continue;
            }
            let hits = match self.db.nodes_by_prop_normalized(
                &scope_set,
                SYNONYM_LABEL,
                SYNONYM_TERM_PROP,
                &PropValue::Str(key),
            ) {
                Ok(h) => h,
                Err(TopoError::Rejected(_)) => continue,
                Err(e) => return Err(classify_topo_error(e)),
            };
            let mut terms: Vec<String> = hits
                .iter()
                .filter_map(|n| match n.props.get(SYNONYM_EXPANSION_PROP) {
                    Some(PropValue::Str(x)) => Some(x.clone()),
                    _ => None,
                })
                .collect();
            terms.sort();
            terms.dedup();
            terms.truncate(4);
            if !terms.is_empty() {
                expansions.push((word.to_string(), terms));
            }
        }
        let options = SearchOptions {
            recency_weight: p.recency_weight,
            recency_half_life_ms: (p.recency_half_life_days * 86_400_000.0) as i64,
            now_ms: None,
            fuzzy_fallback: p.fuzzy,
        };
        let query = RecallQuery {
            // None when the embedder isn't Ready (or errors on this text) —
            // recall then degrades to text/graph legs only.
            vector: self
                .embedder
                .embed(&p.query)
                .map(|v| (self.embedder.model_name(), v)),
            expansions,
            graph_boost: p.graph_boost,
            options,
            labels: Some(p.labels.clone()),
            // Drop memories retired by `remember`'s supersedes; an `as_of`
            // before the retirement still sees them (the mark is a timestamp).
            tombstone_prop: Some(convert::MEMORY_SUPERSEDED_AT_PROP.to_string()),
            text_weight: p.text_weight,
            vector_weight: p.vector_weight,
            graph_weight: p.graph_weight,
            access_weight: p.access_weight,
            ..RecallQuery::new(scope_set, p.query.clone(), p.k)
        };
        // `recall` opens redb read transactions, so unlike the pure snapshot
        // reads it CAN fail with `Storage`/`Encoding` — only its
        // input-validation `Rejected` (k == 0, token-less query, bad recency
        // tuning, weight/labels tuning violations) maps to invalid_params;
        // everything else is a server-side internal_error.
        let hits = self.db.recall(&query).map_err(|e| match e {
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
        // `seed_ids` (non-empty) wins over `seed_id`; at least one is required.
        let seed_strs: Vec<String> = match p.seed_ids {
            Some(ids) if !ids.is_empty() => ids,
            _ => match p.seed_id {
                Some(one) => vec![one],
                None => {
                    return Err(ErrorData::invalid_params(
                        "traverse requires `seed_id` or a non-empty `seed_ids`".to_string(),
                        None,
                    ))
                }
            },
        };
        let mut seeds = Vec::with_capacity(seed_strs.len());
        for s in &seed_strs {
            seeds.push(parse_node_id(s)?);
        }
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
            seeds,
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
        description = "Predict missing links: rank the k nodes this node should probably be connected to but isn't — structurally close (many converging paths) and/or semantically similar (embedding cosine), with shared-neighbor evidence. Each suggestion carries `similarity` (raw cosine when found semantically; null when structural-only) and `common_neighbors` as {id, label, name} objects. Optional min_similarity floors the semantic signal (model-dependent; omit by default). Suggestions only: nothing is created — review them and call link for the ones you agree with, choosing the edge type yourself. Empty when the node is unknown in the read scopes."
    )]
    fn suggest_links(
        &self,
        Parameters(p): Parameters<SuggestLinksParams>,
    ) -> Result<Json<SuggestLinksResult>, ErrorData> {
        let node = parse_node_id(&p.node_id)?;
        let scope_set = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
        let query = topodb::SuggestLinksQuery {
            scopes: scope_set.clone(),
            node,
            k: p.k as usize,
            // Always the active model's namespace: if the embedder is off
            // or the node has no vector, the engine degrades to
            // structure-only — same "visible subset" rule as recall.
            model: Some(self.embedder.model_name()),
            min_semantic_similarity: p.min_similarity,
            as_of: None,
        };
        let hits = self.db.suggest_links(&query).map_err(classify_topo_error)?;
        let suggestions = hits
            .iter()
            .map(|s| {
                let node = convert::node_to_json(&s.node)?;
                // Evidence rendered server-side (host convention — the
                // engine returns ids only): scoped lookups, so an id the
                // scope set cannot see is skipped, never leaked.
                let common_neighbors: Vec<serde_json::Value> = s
                    .common_neighbors
                    .iter()
                    .filter_map(|nid| self.db.node(&scope_set, *nid))
                    .map(|n| {
                        serde_json::json!({
                            "id": n.id.to_string(),
                            "label": n.label.as_str(),
                            "name": display_name(&n),
                        })
                    })
                    .collect();
                Ok(serde_json::json!({
                    "node": node,
                    "score": s.score,
                    "similarity": s.similarity,
                    "common_neighbors": common_neighbors,
                    "structural": s.structural,
                    "semantic": s.semantic,
                }))
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| ErrorData::internal_error(e, None))?;
        Ok(Json(SuggestLinksResult { suggestions }))
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
        description = "Store a linked fact in ONE call: creates the memory, find-or-creates each named entity, and links memory→entity ('about' by default) — atomically, in a single write batch. This is the preferred way to store anything worth remembering. Use the lower-level create_memory / create_entity / link only when you need the pieces separately: an unlinked note, an entity carrying extra props, or entity↔entity relations (works_at, supersede)."
    )]
    fn remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> Result<Json<RememberResult>, ErrorData> {
        let req = convert::RememberRequest {
            content: p.content.clone(),
            entities: p.entities.clone(),
            edge_type: p.edge_type.clone(),
            supersedes: p.supersedes.clone().unwrap_or_default(),
            props: p.props.clone(),
        };
        req.validate()
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        let scope = self.resolve_scope(p.scope.as_deref())?;
        let mut lookup_scopes: Vec<Scope> = self.default_read_scopes.as_slice().to_vec();
        lookup_scopes.push(scope);
        lookup_scopes.push(Scope::Shared);
        let lookup = convert::scopes_to_scope_set(&lookup_scopes);
        let mut plan = convert::plan_remember(&self.db, scope, &lookup, now_ms(), &req).map_err(
            |e| match e {
                convert::ComposeError::Invalid(m) => ErrorData::invalid_params(m, None),
                convert::ComposeError::Engine(t) => classify_topo_error(t),
            },
        )?;
        // Embedder leg (MCP-only): embed the new memory once — the vector
        // serves both the advisory near-duplicate check and the stored
        // embedding — and embed each newly created entity name. Appending
        // after the plan's CreateNode ops keeps SetEmbedding after its node.
        let mut near_duplicates = Vec::new();
        if let Some(content) = plan.new_memory.as_deref() {
            let embedding = self.embedder.embed(content);
            near_duplicates = self.near_duplicates(scope, content, embedding.as_deref());
            if let Some(vector) = embedding {
                plan.ops.push(Op::SetEmbedding {
                    id: plan.memory_id,
                    model: self.embedder.model_name(),
                    vector,
                });
            }
        }
        for (id, name) in &plan.new_entities {
            plan.ops.extend(self.embed_op(*id, name));
        }
        if !plan.ops.is_empty() {
            self.submit_write(plan.ops)?;
        }
        Ok(Json(RememberResult {
            memory_id: plan.memory_id.to_string(),
            entities: plan
                .entities
                .into_iter()
                .map(|e| RememberedEntity {
                    name: e.name,
                    id: e.id.to_string(),
                    created: e.created,
                })
                .collect(),
            edge_ids: plan.edge_ids,
            deduplicated: plan.deduplicated,
            superseded: plan.superseded,
            near_duplicates,
        }))
    }

    #[tool(
        description = "Low-level: store an UNLINKED memory node. Prefer remember, which stores AND links in one atomic call — an unlinked memory can only ever be found by keyword search, never by traversing from the people/projects it concerns. Use this directly only for a deliberately standalone note. content becomes the full-text-searchable body; props holds structured metadata (strings/numbers/bools). Returns the new node's id."
    )]
    fn create_memory(
        &self,
        Parameters(p): Parameters<CreateMemoryParams>,
    ) -> Result<Json<CreateResult>, ErrorData> {
        let scope = self.resolve_scope(p.scope.as_deref())?;
        // Validate reserved keys BEFORE the dedup check (so reserved keys are always rejected).
        let props = convert::memory_props(&p.content, p.props.as_ref())
            .map_err(|e| ErrorData::invalid_params(e, None))?;
        // Dedup: re-storing an identical fact returns the existing node.
        if let Some(existing) = self.existing_memory(scope, &p.content)? {
            return Ok(Json(CreateResult {
                id: existing.to_string(),
                deduplicated: true,
                near_duplicates: Vec::new(),
            }));
        }
        let id = NodeId::new();
        // Embed ONCE and reuse: the vector both searches for semantic near-
        // duplicates (advisory) and is stored on the node. `None` when the
        // embedder isn't Ready — no semantic signal then.
        let embedding = self.embedder.embed(&p.content);
        let near_duplicates = self.near_duplicates(scope, &p.content, embedding.as_deref());
        let mut ops = vec![Op::CreateNode {
            id,
            scope,
            label: MEMORY_LABEL.into(),
            props,
        }];
        if let Some(vector) = embedding {
            ops.push(Op::SetEmbedding {
                id,
                model: self.embedder.model_name(),
                vector,
            });
        }
        self.submit_write(ops)?;
        Ok(Json(CreateResult {
            id: id.to_string(),
            deduplicated: false,
            near_duplicates,
        }))
    }

    #[tool(
        description = "Find-or-create an entity node (person, project, concept). remember calls this resolution for you when storing a fact; call it directly when an entity needs extra props, or to get an id for entity↔entity link calls. The name is matched case- and whitespace-insensitively across the read scopes, the write scope, AND shared — if the entity already exists anywhere visible, its id is returned with created: false and NO duplicate is made (any new props keys are merged; existing keys are never overwritten). Use one canonical name form per entity (prefer the fullest name you know, e.g. 'Drew Powell' over 'Drew') so future mentions keep resolving to the same node."
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

        let existing = self.find_existing_entity(scope, &p.name)?;

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
        // Create path only: the matched/upsert path above embeds nothing —
        // the canonical node either already has its vector or backfill
        // covers it.
        let embed = self.embed_op(id, &p.name);
        let mut ops = vec![Op::CreateNode {
            id,
            scope,
            label: ENTITY_LABEL.into(),
            props,
        }];
        ops.extend(embed);
        self.submit_write(ops)?;
        Ok(Json(UpsertResult {
            id: id.to_string(),
            created: true,
        }))
    }

    #[tool(
        description = "Register an alternate name for an existing entity ('Drew' for 'Drew Powell', 'the broker' for 'launch.js'). From then on create_entity, find_by_prop, and search resolve the alias to the canonical entity — use this the moment you learn a second name for something instead of creating a duplicate. Errors if the alias already names a DIFFERENT entity (that's a merge situation; both ids are reported). Idempotent for the same entity. Remove an alias with remove_node on the alias node id."
    )]
    fn add_alias(
        &self,
        Parameters(p): Parameters<AddAliasParams>,
    ) -> Result<Json<UpsertResult>, ErrorData> {
        let entity_id = parse_node_id(&p.entity_id)?;
        // Read set for validation: default read scopes + shared (aliases can
        // point at shared entities).
        let mut lookup: Vec<Scope> = self.default_read_scopes.as_slice().to_vec();
        lookup.push(Scope::Shared);
        let read_set = convert::scopes_to_scope_set(&lookup);

        let Some(target) = self.db.node(&read_set, entity_id) else {
            return Err(ErrorData::invalid_params(
                format!("entity {} not found in the read scopes", p.entity_id),
                None,
            ));
        };
        if target.label != ENTITY_LABEL {
            return Err(ErrorData::invalid_params(
                format!(
                    "add_alias target must be an Entity, {} is a {}",
                    p.entity_id, target.label
                ),
                None,
            ));
        }
        // Conflict: alias equal to a different entity's name or alias. A
        // custom spec without (Entity, name) equality-indexed can't check
        // for a conflict — degrade to "no conflict" rather than failing the
        // write, same as create_entity's dedup lookup.
        let existing = match self.resolve_entities_by_name(&read_set, &p.alias) {
            Ok(hits) => hits,
            Err(TopoError::Rejected(_)) => Vec::new(),
            Err(e) => return Err(classify_topo_error(e)),
        };
        if let Some(other) = existing.iter().find(|n| n.id != entity_id) {
            return Err(ErrorData::invalid_params(
                format!(
                    "\"{}\" already resolves to entity {} — adding it as an alias of {} \
                     would make the name ambiguous. If they are the same thing, merge \
                     them (relink + remove_node) instead.",
                    p.alias, other.id, entity_id
                ),
                None,
            ));
        }
        // Idempotency: an Alias node with this name already pointing here?
        let alias_hits = self
            .db
            .nodes_by_prop_normalized(
                &read_set,
                ALIAS_LABEL,
                ALIAS_NAME_PROP,
                &PropValue::Str(p.alias.clone()),
            )
            .map_err(classify_topo_error)?;
        for a in &alias_hits {
            let edges = self
                .db
                .edges_from(
                    &read_set,
                    a.id,
                    Some(entity_id),
                    Some(ALIAS_EDGE_TYPE),
                    true,
                )
                .map_err(classify_topo_error)?;
            if !edges.is_empty() {
                return Ok(Json(UpsertResult {
                    id: a.id.to_string(),
                    created: false,
                }));
            }
        }
        // Create alias node + alias_of edge atomically. Scope defaults to
        // the ENTITY's scope so the pair travels together.
        let scope = match p.scope.as_deref() {
            Some(s) => self.resolve_scope(Some(s))?,
            None => target.scope,
        };
        let alias_id = NodeId::new();
        // Embed before `p.alias` moves into the props map below.
        let embed = self.embed_op(alias_id, &p.alias);
        let mut props = Props::new();
        props.insert(ALIAS_NAME_PROP.to_string(), PropValue::Str(p.alias));
        let mut ops = vec![
            Op::CreateNode {
                id: alias_id,
                scope,
                label: ALIAS_LABEL.into(),
                props,
            },
            Op::CreateEdge {
                id: EdgeId::new(),
                scope,
                ty: ALIAS_EDGE_TYPE.into(),
                from: alias_id,
                to: entity_id,
                props: Props::new(),
                valid_from: None,
            },
        ];
        ops.extend(embed);
        self.submit_write(ops)?;
        Ok(Json(UpsertResult {
            id: alias_id.to_string(),
            created: true,
        }))
    }

    #[tool(
        description = "Teach search a domain equivalence: after add_synonym('auth','login'), searching 'auth' also matches memories that say 'login' (at a discount, so exact matches still win). Bidirectional by default. Use when you learn this project's vocabulary — 'broker' meaning launch.js, 'the engine' meaning crates/topodb. Depth-1 only: synonyms never chain. Remove with remove_node on the synonym node id."
    )]
    fn add_synonym(
        &self,
        Parameters(p): Parameters<AddSynonymParams>,
    ) -> Result<Json<AddSynonymResult>, ErrorData> {
        // Terms are stored in ANALYZED (stemmed, lowercased) form so
        // query-time lookup — which analyzes the query word the same way —
        // can never miss a morphological variant. Expansions stay raw
        // (trimmed): the engine tokenizes them at scoring time.
        let term = topodb::analyze(&p.term)
            .into_iter()
            .next()
            .unwrap_or_default();
        let expansion = p.expansion.trim().to_lowercase();
        let expansion_key = topodb::analyze(&expansion)
            .into_iter()
            .next()
            .unwrap_or_default();
        if term.is_empty() || expansion_key.is_empty() {
            return Err(ErrorData::invalid_params(
                "term and expansion must each contain at least one word",
                None,
            ));
        }
        if term == expansion_key {
            return Err(ErrorData::invalid_params(
                format!(
                    "term and expansion reduce to the same word ({term:?}) — a self-synonym does nothing"
                ),
                None,
            ));
        }
        let scope = self.resolve_scope(p.scope.as_deref())?;
        let read_set = convert::scope_to_scope_set(scope);
        let mut ids = Vec::new();
        let mut created = false;
        // Reverse direction stores the ANALYZED expansion as its term and
        // the raw term text as its expansion — both directions must be
        // lookup-able by analyzed key.
        let raw_term = p.term.trim().to_lowercase();
        let pairs: Vec<(String, String)> = if p.bidirectional {
            vec![(term.clone(), expansion.clone()), (expansion_key, raw_term)]
        } else {
            vec![(term, expansion)]
        };
        for (t, e) in pairs {
            // Idempotent per direction: existing (term, expansion) pair reused.
            let existing = self
                .db
                .nodes_by_prop_normalized(
                    &read_set,
                    SYNONYM_LABEL,
                    SYNONYM_TERM_PROP,
                    &PropValue::Str(t.clone()),
                )
                .map_err(classify_topo_error)?;
            if let Some(node) = existing.iter().find(|n| {
                matches!(n.props.get(SYNONYM_EXPANSION_PROP), Some(PropValue::Str(x)) if x == &e)
            }) {
                ids.push(node.id.to_string());
                continue;
            }
            let id = NodeId::new();
            let mut props = Props::new();
            props.insert(SYNONYM_TERM_PROP.to_string(), PropValue::Str(t));
            props.insert(SYNONYM_EXPANSION_PROP.to_string(), PropValue::Str(e));
            self.submit_write(vec![Op::CreateNode {
                id,
                scope,
                label: SYNONYM_LABEL.into(),
                props,
            }])?;
            ids.push(id.to_string());
            created = true;
        }
        Ok(Json(AddSynonymResult { ids, created }))
    }

    #[tool(
        description = "Create (or reuse) a typed, time-aware edge between two existing nodes. remember already links memories to their entities — use this for entity↔entity relations ('works_on', 'works_at') and custom memory links. edge_type is normalized (lowercased; spaces/hyphens collapse to '_', so 'Works At' == 'works_at'); reuse existing type names rather than inventing synonyms ('works_at', not also 'employed_by'). Calling link again with the same from/to/type returns the existing open edge (created: false) instead of a duplicate. When the new fact REPLACES the old one for a to-one relation (moved teams, changed employer), pass supersede: true to atomically close the other open same-type edges from this node. Errors if either node doesn't exist. When linking shared-scope nodes, pass scope: 'shared' or the edge is invisible outside this project."
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
                 separately. Storing well: use remember (one atomic call: memory + \
                 find-or-create entities + links); the primitives remain for the exceptions — \
                 create_memory for a deliberately unlinked note, create_entity when an entity \
                 needs extra props, link for entity↔entity relations and supersede: true when \
                 a to-one fact changes. Recalling well: search_memories stems \
                 terms, falls back to close prefix/typo matches, and expands learned \
                 synonyms (add_synonym) automatically — but it can't guess vocabulary it \
                 was never taught, so retry with different words before concluding \
                 nothing is stored — then traverse from the best hit; use \
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

#[cfg(test)]
mod dup_classify_tests {
    use super::{dup_band, dup_relation, is_supersession};

    // Labeled battery from the calibration experiment (raw cosine can't separate
    // these — the negation cue must). SAME/UNRELATED => "duplicate" relation,
    // CONTRADICT => "supersession".
    #[test]
    fn supersession_detector_separates_contradictions_from_restatements() {
        let same = [
            ("The team chose redb as TopoDB's storage engine for its single-file ACID guarantees",
             "TopoDB persists its data in the redb embedded key-value database"),
            ("TopoDB uses redb as its storage backend", "The storage engine behind TopoDB is redb"),
            ("Drew prefers Colima over Docker Desktop",
             "Drew runs containers on Colima instead of Docker Desktop"),
            ("CI runs fmt, clippy, and tests on ubuntu and windows",
             "The CI pipeline executes formatting, linting, and the test suite on both ubuntu and windows runners"),
            ("the auth service issues JWT tokens to sign in users",
             "auth uses JSON Web Tokens to authenticate and log people in"),
            // Post-nominal negation that AGREES with a pre-nominal one — both say
            // Windows is gone — must read as a duplicate, not a contradiction.
            ("CI runs only on ubuntu (windows dropped)",
             "CI no longer runs on windows, only ubuntu"),
        ];
        let contradict = [
            (
                "TopoDB stores its data in redb",
                "TopoDB now stores its data in sled, not redb",
            ),
            (
                "the auth service issues JWT tokens",
                "the auth service now issues opaque session tokens, not JWTs",
            ),
            (
                "CI runs on ubuntu and windows",
                "CI no longer runs on windows, only ubuntu",
            ),
            // Post-nominal negation ("... was removed") must fire too.
            (
                "the redb backend is used for storage",
                "the redb backend was removed",
            ),
        ];
        for (a, b) in same {
            assert!(
                !is_supersession(a, b),
                "should read as a duplicate: {a:?} / {b:?}"
            );
            assert_eq!(dup_relation(a, b), "duplicate");
        }
        for (a, b) in contradict {
            assert!(
                is_supersession(a, b),
                "should read as a supersession: {a:?} / {b:?}"
            );
            assert_eq!(dup_relation(a, b), "supersession");
        }
    }

    #[test]
    fn is_supersession_is_symmetric() {
        let a = "TopoDB stores its data in redb";
        let b = "TopoDB now stores its data in sled, not redb";
        assert_eq!(is_supersession(a, b), is_supersession(b, a));
    }

    #[test]
    fn band_splits_at_the_strong_floor() {
        assert_eq!(dup_band(0.95), "likely");
        assert_eq!(dup_band(0.80), "likely");
        assert_eq!(dup_band(0.799), "possible");
        assert_eq!(dup_band(0.70), "possible");
    }
}
