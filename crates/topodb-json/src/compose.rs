//! Composed write planning shared by TopoDB's front ends: the `remember`
//! verb (store + find-or-create + link + supersede as ONE atomic batch) and
//! the entity/memory lookups it is built from. Unlike the rest of this
//! crate, functions here READ from a `Db` — but never write: every function
//! returns planned `Op`s (or a lookup result) and the caller submits.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;
use topodb::{Db, EdgeId, NodeId, NodeRecord, Op, PropValue, Props, Scope, ScopeSet, TopoError};

use crate::{
    merge_required_prop, normalize_edge_type, scopes_to_scope_set, ALIAS_EDGE_TYPE, ALIAS_LABEL,
    ALIAS_NAME_PROP, ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_HASH_PROP, MEMORY_CONTENT_PROP,
    MEMORY_LABEL, MEMORY_SUPERSEDED_AT_PROP,
};

/// Edge type `remember` uses when the caller doesn't name one.
pub const DEFAULT_REMEMBER_EDGE_TYPE: &str = "about";

/// Planning failure: `Invalid` is a caller-fixable input problem (surface the
/// message verbatim); `Engine` is a database failure unrelated to the input.
#[derive(Debug)]
pub enum ComposeError {
    Invalid(String),
    Engine(TopoError),
}

impl From<TopoError> for ComposeError {
    fn from(e: TopoError) -> Self {
        ComposeError::Engine(e)
    }
}

// --- moved verbatim from topodb-mcp/src/server.rs (self.db -> db) ---
// Keep the original doc comments from server.rs on each of these when moving.

/// lowercased — mirroring the engine's prop-index normalization
/// (`prop_index::normalize_str`, which is pub(crate) and thus can't be
/// called from here). Drift between the two only weakens IN-CALL dedup
/// (["Drew", "drew"] in one call); cross-call dedup always goes through the
/// engine's own normalized index via find_existing_entity.
pub fn entity_dedup_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Normalize memory content for dedup: trim and collapse internal whitespace.
/// Deliberately NOT lowercased — casing can carry meaning in a stored fact.
pub fn normalize_content(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable FNV-1a 64-bit hash of normalized content, hex-encoded. PERSISTED
/// (equality-indexed as `content_hash`) — the algorithm must never change.
/// Collisions are harmless: dedup always verifies exact normalized content.
pub fn content_hash(content: &str) -> String {
    let normalized = normalize_content(content);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalized.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// (Entity, name) matches followed through alias_of. Deduped by id,
/// oldest first.
///
/// Returns the raw `TopoError` (not `ErrorData`) rather than swallowing
/// it: the two existing call sites disagree on what an undeclared
/// (Entity, name) index should mean. `find_by_prop` must still surface it
/// as a caller error — that is the exact contract tests pin down (an
/// undeclared-index probe on a custom spec must error, not silently return
/// empty, or a clobbered spec reopen would go undetected). `create_entity`
/// instead treats it as "can't dedup on this spec" and degrades to
/// create-always. Only the (Alias, name) probe's `Rejected` is
/// unconditionally swallowed here — a spec that predates the Alias index
/// (or a custom spec that never declared it) simply has no aliases to
/// resolve, which is never a caller error.
pub fn resolve_entities_by_name(
    db: &Db,
    scopes: &ScopeSet,
    name: &str,
) -> Result<Vec<NodeRecord>, TopoError> {
    let value = PropValue::Str(name.to_string());
    let mut out = db.nodes_by_prop_normalized(scopes, ENTITY_LABEL, ENTITY_NAME_PROP, &value)?;
    let aliases = match db.nodes_by_prop_normalized(scopes, ALIAS_LABEL, ALIAS_NAME_PROP, &value) {
        Ok(hits) => hits,
        Err(TopoError::Rejected(_)) => Vec::new(),
        Err(e) => return Err(e),
    };
    for alias in aliases {
        for edge in db.edges_from(scopes, alias.id, None, Some(ALIAS_EDGE_TYPE), true)? {
            if let Some(canonical) = db.node(scopes, edge.to) {
                if canonical.label == ENTITY_LABEL {
                    out.push(canonical);
                }
            }
        }
    }
    out.sort_by_key(|n| n.id);
    out.dedup_by_key(|n| n.id);
    Ok(out)
}

/// `lookup` is the caller's collision surface (MCP: default read scopes +
/// write scope + shared; CLI: write scope + shared). `Ok(None)` means
/// "create it" — covering both no-visible-match and a custom spec without
/// the (Entity, name) equality index (`Rejected`), which degrades to
/// create-always rather than failing the write.
pub fn find_existing_entity(
    db: &Db,
    lookup: &ScopeSet,
    name: &str,
) -> Result<Option<NodeRecord>, TopoError> {
    match resolve_entities_by_name(db, lookup, name) {
        Ok(hits) => Ok(hits.into_iter().min_by_key(|n| n.id)),
        Err(TopoError::Rejected(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The id of a Memory in `write_scope` whose normalized content equals
/// `content`. Hash-bucket lookup, then exact normalized-content verify on
/// every candidate; oldest id wins.
pub fn existing_memory(
    db: &Db,
    write_scope: Scope,
    content: &str,
) -> Result<Option<NodeId>, TopoError> {
    let hash = content_hash(content);
    let want = normalize_content(content);
    let scope_set = scopes_to_scope_set(&[write_scope]);
    let candidates = db.nodes_by_prop(
        &scope_set,
        MEMORY_LABEL,
        MEMORY_CONTENT_HASH_PROP,
        &PropValue::Str(hash),
    )?;
    Ok(candidates
        .into_iter()
        .filter(|n| {
            matches!(n.props.get(MEMORY_CONTENT_PROP), Some(PropValue::Str(c)) if normalize_content(c) == want)
        })
        .min_by_key(|n| n.id)
        .map(|n| n.id))
}

/// Ops marking `ids` superseded (stamp + close open out-edges) plus the ids
/// actually marked. Moved from server.rs `supersede_ops`; `now_ms` is a
/// parameter so tests are deterministic. Error strings must stay identical.
fn plan_supersede(
    db: &Db,
    write_scope: Scope,
    ids: &[String],
    now_ms: i64,
) -> Result<(Vec<Op>, Vec<String>), ComposeError> {
    let mut ops = Vec::new();
    let mut marked = Vec::new();
    if ids.is_empty() {
        return Ok((ops, marked));
    }
    let scope_set = scopes_to_scope_set(&[write_scope]);
    let mut seen = BTreeSet::new();
    for raw in ids {
        let id: NodeId = raw
            .parse()
            .map_err(|e| ComposeError::Invalid(format!("invalid node id {raw:?}: {e}")))?;
        if !seen.insert(id) {
            continue;
        }
        let node = db.node(&scope_set, id).ok_or_else(|| {
            ComposeError::Invalid(format!(
                "supersedes id {raw} is not a node in the write scope"
            ))
        })?;
        if node.label != MEMORY_LABEL {
            return Err(ComposeError::Invalid(format!(
                "supersedes id {raw} is a {}, not a Memory",
                node.label
            )));
        }
        if node.props.contains_key(MEMORY_SUPERSEDED_AT_PROP) {
            continue;
        }
        let mut props: BTreeMap<String, Option<PropValue>> = BTreeMap::new();
        props.insert(
            MEMORY_SUPERSEDED_AT_PROP.into(),
            Some(PropValue::Int(now_ms)),
        );
        ops.push(Op::SetNodeProps { id, props });
        for e in db.edges_from(&scope_set, id, None, None, true)? {
            ops.push(Op::CloseEdge {
                id: e.id,
                valid_to: None,
            });
        }
        marked.push(id.to_string());
    }
    Ok((ops, marked))
}

// --- the composed verb ---

pub struct RememberRequest {
    pub content: String,
    pub entities: Vec<String>,
    pub edge_type: Option<String>,
    pub supersedes: Vec<String>,
    /// Extra memory metadata as a JSON object (same contract as
    /// `merge_required_prop`'s `extra`).
    pub props: Option<Value>,
}

impl RememberRequest {
    /// Input-only validation (no db access): the edge type normalizes and
    /// at least one non-blank entity name is present. `plan_remember` runs
    /// this itself; front ends call it FIRST when they must report input
    /// errors ahead of scope/db errors (the pre-refactor precedence).
    /// Returns the normalized edge type.
    pub fn validate(&self) -> Result<String, String> {
        let ty = normalize_edge_type(
            self.edge_type
                .as_deref()
                .unwrap_or(DEFAULT_REMEMBER_EDGE_TYPE),
        )?;
        if self.entities.is_empty() {
            return Err(
                "entities must contain at least one name — use create_memory for a deliberately unlinked note".into(),
            );
        }
        if self.entities.iter().any(|n| n.trim().is_empty()) {
            return Err("entity names must be non-empty".into());
        }
        Ok(ty)
    }
}

pub struct PlannedEntity {
    pub name: String,
    pub id: NodeId,
    pub created: bool,
}

pub struct RememberPlan {
    /// ONE atomic batch; possibly empty (pure no-op). The caller submits.
    pub ops: Vec<Op>,
    pub memory_id: NodeId,
    pub deduplicated: bool,
    /// The content, iff a new Memory node is planned (callers with an
    /// embedder append `SetEmbedding` ops keyed on this).
    pub new_memory: Option<String>,
    /// (id, name) of every planned Entity create, for the same purpose.
    pub new_entities: Vec<(NodeId, String)>,
    pub entities: Vec<PlannedEntity>,
    pub edge_ids: Vec<String>,
    pub superseded: Vec<String>,
}

pub fn plan_remember(
    db: &Db,
    write_scope: Scope,
    lookup: &ScopeSet,
    now_ms: i64,
    req: &RememberRequest,
) -> Result<RememberPlan, ComposeError> {
    let ty = req.validate().map_err(ComposeError::Invalid)?;
    let existing = existing_memory(db, write_scope, &req.content)?;
    let deduplicated = existing.is_some();
    let memory_id = existing.unwrap_or_else(NodeId::new);
    let (supersede_ops, superseded) = plan_supersede(db, write_scope, &req.supersedes, now_ms)?;

    struct Resolved {
        name: String,
        id: NodeId,
        created: bool,
        op: Option<Op>,
    }
    let mut seen = BTreeSet::new();
    let mut resolved: Vec<Resolved> = Vec::new();
    for name in req
        .entities
        .iter()
        .filter(|n| seen.insert(entity_dedup_key(n)))
    {
        match find_existing_entity(db, lookup, name)? {
            Some(node) => resolved.push(Resolved {
                name: name.clone(),
                id: node.id,
                created: false,
                op: None,
            }),
            None => {
                let id = NodeId::new();
                let props =
                    merge_required_prop(ENTITY_NAME_PROP, PropValue::Str(name.clone()), None)
                        .map_err(ComposeError::Invalid)?;
                resolved.push(Resolved {
                    name: name.clone(),
                    id,
                    created: true,
                    op: Some(Op::CreateNode {
                        id,
                        scope: write_scope,
                        label: ENTITY_LABEL.into(),
                        props,
                    }),
                });
            }
        }
    }

    let mut ops: Vec<Op> = Vec::new();
    let mut new_memory = None;
    if !deduplicated {
        let hash = content_hash(&req.content);
        let mut props = merge_required_prop(
            MEMORY_CONTENT_PROP,
            PropValue::Str(req.content.clone()),
            req.props.as_ref(),
        )
        .map_err(ComposeError::Invalid)?;
        props.insert(MEMORY_CONTENT_HASH_PROP.into(), PropValue::Str(hash));
        ops.push(Op::CreateNode {
            id: memory_id,
            scope: write_scope,
            label: MEMORY_LABEL.into(),
            props,
        });
        new_memory = Some(req.content.clone());
    }

    // Two names resolving to one node (e.g. via alias) collapse to one edge.
    let mut seen_ids = BTreeSet::new();
    resolved.retain(|r| seen_ids.insert(r.id));

    // On a dedup hit, entities already linked keep their edge.
    let already_linked: HashMap<NodeId, EdgeId> = if deduplicated {
        let scope_set = scopes_to_scope_set(&[write_scope]);
        db.edges_from(&scope_set, memory_id, None, Some(ty.as_str()), true)?
            .into_iter()
            .map(|e| (e.to, e.id))
            .collect()
    } else {
        HashMap::new()
    };

    let mut entities = Vec::with_capacity(resolved.len());
    let mut edge_ids = Vec::with_capacity(resolved.len());
    let mut new_entities = Vec::new();
    for r in resolved {
        if let Some(op) = r.op {
            new_entities.push((r.id, r.name.clone()));
            ops.push(op);
        }
        let edge_id = match already_linked.get(&r.id) {
            Some(existing_edge) => existing_edge.to_string(),
            None => {
                let id = EdgeId::new();
                ops.push(Op::CreateEdge {
                    id,
                    scope: write_scope,
                    ty: ty.clone().into(),
                    from: memory_id,
                    to: r.id,
                    props: Props::new(),
                    valid_from: None,
                });
                id.to_string()
            }
        };
        edge_ids.push(edge_id);
        entities.push(PlannedEntity {
            name: r.name,
            id: r.id,
            created: r.created,
        });
    }
    ops.extend(supersede_ops);
    Ok(RememberPlan {
        ops,
        memory_id,
        deduplicated,
        new_memory,
        new_entities,
        entities,
        edge_ids,
        superseded,
    })
}
