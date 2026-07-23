//! JSON ↔ engine-type conversions shared by TopoDB's JSON-speaking front ends
//! (currently `topodb-mcp`; `topodb-cli` is next).
//!
//! Most functions here are pure (no I/O, no `Db` access) and return `Result<_,
//! String>` — callers are responsible for mapping the `String` into their own
//! error type (`topodb-mcp`'s `server.rs` maps it to an `rmcp::ErrorData`:
//! `invalid_params` for bad input, `internal_error` otherwise). Nothing here
//! ever panics: an unrepresentable value is always an `Err`, never an
//! `unwrap`/`expect`. Exception: the `compose` module reads from a `Db` to
//! plan writes, but never writes itself.

mod batch;
pub use batch::resolve_batch;

mod compose;
pub use compose::{
    content_hash, entity_dedup_key, existing_memory, find_existing_entity, normalize_content,
    plan_remember, resolve_entities_by_name, ComposeError, PlannedEntity, RememberPlan,
    RememberRequest, DEFAULT_REMEMBER_EDGE_TYPE,
};

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::str::FromStr;
use topodb::{
    EdgeRecord, IndexSpec, NodeRecord, PropIndex, PropValue, Props, Scope, ScopeId, ScopeSet,
    Subgraph,
};

/// Label/prop name constants for the two built-in write shapes
/// (`create_memory`/`create-memory`, `create_entity`/`create-entity`). Single
/// source of truth shared by every front end (`topodb-mcp`'s default
/// [`IndexSpec`](topodb::IndexSpec) and write tools, `topodb-cli`'s
/// `create-entity`/`create-memory` subcommands) so writes land on exactly the
/// `(label, prop)` pairs the default spec indexes — search and lookup work
/// out of the box regardless of which front end wrote the data.
pub const ENTITY_LABEL: &str = "Entity";
pub const ENTITY_NAME_PROP: &str = "name";
pub const MEMORY_LABEL: &str = "Memory";
pub const MEMORY_CONTENT_PROP: &str = "content";
/// Equality-indexed hash of a memory's normalized content, used to dedup a
/// re-stored fact to the existing node instead of minting a duplicate. Set by
/// the write front ends (`remember`/`create_memory`), never by a caller.
pub const MEMORY_CONTENT_HASH_PROP: &str = "content_hash";
/// Millisecond timestamp at which a memory was superseded by a newer fact.
/// Set by `remember`'s `supersedes`; recall drops a memory whose value here is
/// `<=` the query's `now` (so an `as_of` before it still sees the old fact).
/// The node is not deleted — supersession dates a fact, keeping its history.
pub const MEMORY_SUPERSEDED_AT_PROP: &str = "superseded_at";
pub const ALIAS_LABEL: &str = "Alias";
pub const ALIAS_NAME_PROP: &str = "name";
pub const ALIAS_EDGE_TYPE: &str = "alias_of";
pub const SYNONYM_LABEL: &str = "Synonym";
pub const SYNONYM_TERM_PROP: &str = "term";
pub const SYNONYM_EXPANSION_PROP: &str = "expansion";

/// The ONE canonical default [`IndexSpec`] for TopoDB's built-in write shapes,
/// shared by every front end (`topodb-mcp` when `--spec` is omitted;
/// `topodb-cli` when it creates a brand-new db file). Declares equality on
/// `(Entity, name)`, `(Alias, name)`, and `(Synonym, term)`, and text on
/// `(Memory, content)`, `(Entity, name)`, and `(Alias, name)`, using the shared
/// label and property constants.
///
/// Single-sourcing this is load-bearing: because a CLI-created db and an
/// MCP-created db are opened with a *byte-identical* persisted `index_spec`,
/// either front end can later serve a db the other created via `open_stored`
/// without triggering an FTS reindex or mis-declaring the equality index — and
/// both `find` (equality on `Entity`/`name`) and `search` (text on
/// `Memory`/`content`) work out of the box on a fresh db regardless of which
/// tool wrote it.
pub fn default_spec() -> IndexSpec {
    IndexSpec {
        equality: vec![
            PropIndex {
                label: ENTITY_LABEL.into(),
                prop: ENTITY_NAME_PROP.into(),
            },
            // Aliases resolve exactly like entity names (upsert/find probe
            // both); synonym terms are looked up per query word.
            PropIndex {
                label: ALIAS_LABEL.into(),
                prop: ALIAS_NAME_PROP.into(),
            },
            PropIndex {
                label: SYNONYM_LABEL.into(),
                prop: SYNONYM_TERM_PROP.into(),
            },
            // A memory's normalized-content hash, so a re-stored fact resolves
            // to its existing node (content-verified) instead of duplicating.
            PropIndex {
                label: MEMORY_LABEL.into(),
                prop: MEMORY_CONTENT_HASH_PROP.into(),
            },
        ],
        text: vec![
            PropIndex {
                label: MEMORY_LABEL.into(),
                prop: MEMORY_CONTENT_PROP.into(),
            },
            // Entity names are text-indexed too (not just equality-indexed)
            // so `search_memories` can find an entity by name — without
            // this, a search for "Drew" returns only Memory nodes whose
            // content happens to mention the name, and the entity itself is
            // reachable only by exact `find_by_prop`.
            PropIndex {
                label: ENTITY_LABEL.into(),
                prop: ENTITY_NAME_PROP.into(),
            },
            PropIndex {
                label: ALIAS_LABEL.into(),
                prop: ALIAS_NAME_PROP.into(),
            },
        ],
    }
}

/// Every stock spec generation this crate has ever shipped, oldest first.
/// A persisted spec equal (order-insensitively) to ANY of them upgrades to
/// the current `default_spec`; anything else is a customization and is
/// returned unchanged.
fn stock_generations() -> Vec<IndexSpec> {
    // g0 (pre-0.0.9): equality (Entity, name); text (Memory, content).
    let g0 = IndexSpec {
        equality: vec![PropIndex {
            label: ENTITY_LABEL.into(),
            prop: ENTITY_NAME_PROP.into(),
        }],
        text: vec![PropIndex {
            label: MEMORY_LABEL.into(),
            prop: MEMORY_CONTENT_PROP.into(),
        }],
    };
    // g1: g0 + text (Entity, name).
    let g1 = IndexSpec {
        equality: g0.equality.clone(),
        text: vec![
            PropIndex {
                label: MEMORY_LABEL.into(),
                prop: MEMORY_CONTENT_PROP.into(),
            },
            PropIndex {
                label: ENTITY_LABEL.into(),
                prop: ENTITY_NAME_PROP.into(),
            },
        ],
    };
    vec![g0, g1]
}

/// Maps a db's persisted spec forward when — and only when — it is exactly a
/// stock default this crate has shipped: any recognized stock generation
/// upgrades to the current [`default_spec`]. Any other spec — a `--spec`
/// customization, however small — is returned unchanged: silently rewriting a
/// declared spec would reindex data behind its owner's back. Comparison is
/// order-insensitive, matching how the engine's `ensure_index_spec` compares
/// specs (it sorts both lists before persisting).
pub fn upgraded_spec(persisted: IndexSpec) -> IndexSpec {
    let sorted = |spec: &IndexSpec| {
        let mut eq: Vec<(String, String)> = spec
            .equality
            .iter()
            .map(|p| (p.label.to_string(), p.prop.clone()))
            .collect();
        let mut text: Vec<(String, String)> = spec
            .text
            .iter()
            .map(|p| (p.label.to_string(), p.prop.clone()))
            .collect();
        eq.sort();
        text.sort();
        (eq, text)
    };
    let p = sorted(&persisted);
    if stock_generations().iter().any(|g| sorted(g) == p) {
        default_spec()
    } else {
        persisted
    }
}

/// Canonical form for edge types: Unicode-lowercased, with runs of
/// whitespace, hyphens, and underscores collapsed to a single underscore
/// (leading/trailing separators dropped). `"Works At"`, `"works-at"`, and
/// `"works_at"` all normalize to `"works_at"` — one relation, one vocabulary
/// entry, instead of three parallel edge types that silently fragment
/// traversal filters. Every front-end write path (`link` tool, batch `link`
/// command, CLI) passes edge types through here; read-side type filters
/// should probe both the raw and normalized forms so edges written before
/// normalization stay reachable. `Err` on a type that normalizes to empty.
pub fn normalize_edge_type(raw: &str) -> Result<String, String> {
    let lowered = raw.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending_sep = false;
    for c in lowered.chars() {
        if c.is_whitespace() || c == '-' || c == '_' {
            if !out.is_empty() {
                pending_sep = true;
            }
        } else {
            if pending_sep {
                out.push('_');
                pending_sep = false;
            }
            out.push(c);
        }
    }
    if out.is_empty() {
        return Err(format!(
            "edge type {raw:?} is empty once normalized (lowercase, separators collapsed to '_')"
        ));
    }
    Ok(out)
}

/// Human/JSON-facing rendering of a [`Scope`]: `"shared"` or the ULID string.
/// Reused by every front end's `info`/`db_info`-style output and scope
/// round-tripping. (Distinct from [`scope_to_json`], which wraps the same
/// rendering in a `serde_json::Value` for a JSON response body; this returns
/// a bare `String` for contexts — like a struct field — that want the label
/// without a `Value` wrapper.)
pub fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Shared => "shared".to_string(),
        Scope::Id(id) => id.to_string(),
    }
}

/// Error string for both directions of an unrepresentable [`PropValue`]:
/// `Bytes` and `DateTime` have no JSON counterpart over MCP v0, and any JSON
/// shape that isn't a string/number/bool (array, object, null) has no
/// [`PropValue`] counterpart either.
pub const UNSUPPORTED: &str = "unsupported over MCP v0";

/// `PropValue` → `serde_json::Value`. `Str`/`Int`/`Bool` map directly; `Float`
/// maps to a JSON number (`Err` only for a non-finite float, which JSON has no
/// representation for). `Bytes`/`DateTime` are [`UNSUPPORTED`].
pub fn prop_value_to_json(v: &PropValue) -> Result<Value, String> {
    match v {
        PropValue::Str(s) => Ok(Value::String(s.clone())),
        PropValue::Int(i) => Ok(Value::Number((*i).into())),
        PropValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .ok_or_else(|| format!("{UNSUPPORTED}: non-finite float")),
        PropValue::Bool(b) => Ok(Value::Bool(*b)),
        PropValue::Bytes(_) | PropValue::DateTime(_) => Err(UNSUPPORTED.to_string()),
    }
}

/// `serde_json::Value` → `PropValue`. Strings/bools map directly. A JSON
/// integer maps to `Int` when it fits `i64`, and is an error when it doesn't
/// (`(i64::MAX, u64::MAX]` — silently downgrading it to a lossy `Float` would
/// corrupt the value); only a genuine non-integer number maps to `Float`.
/// This is the inverse of `prop_value_to_json`'s `Int`/`Float` handling.
/// Every other JSON shape (array, object, null — and, structurally, anything
/// that would have needed to round-trip through `Bytes`/`DateTime`) is
/// [`UNSUPPORTED`].
pub fn json_to_prop_value(v: &Value) -> Result<PropValue, String> {
    match v {
        Value::String(s) => Ok(PropValue::Str(s.clone())),
        Value::Bool(b) => Ok(PropValue::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(PropValue::Int(i))
            } else if n.is_u64() {
                // An integer above i64::MAX: representable in JSON but not in
                // PropValue::Int, and f64 can't hold it losslessly either.
                Err(format!("integer out of supported range (max {})", i64::MAX))
            } else if let Some(f) = n.as_f64() {
                Ok(PropValue::Float(f))
            } else {
                Err(format!("{UNSUPPORTED}: number out of range"))
            }
        }
        Value::Array(_) | Value::Object(_) | Value::Null => Err(UNSUPPORTED.to_string()),
    }
}

/// `Props` (a `BTreeMap<String, PropValue>`) → a JSON object, propagating the
/// first unrepresentable value as `Err`.
pub fn props_to_json(props: &Props) -> Result<Value, String> {
    let mut map = Map::with_capacity(props.len());
    for (k, v) in props {
        map.insert(k.clone(), prop_value_to_json(v)?);
    }
    Ok(Value::Object(map))
}

/// A JSON object → `Props`, propagating the first unrepresentable value as
/// `Err`. `Err` if `v` isn't a JSON object at all.
///
/// The inverse of `props_to_json`; `topodb-mcp`'s write tools (`create_entity`
/// / `create_memory` / `link`) call this on the caller-supplied `props`
/// object.
///
/// **v0 limitation:** a JSON integer literal below `i64::MIN` (e.g.
/// `-99999999999999999999`) is *already* an `f64` by the time it reaches
/// [`json_to_prop_value`] — `serde_json`'s parser itself has no `i64`-sized
/// negative bucket wide enough to hold it, so it falls back to a lossy float
/// at parse time, upstream of anything this module can inspect or reject
/// (unlike the positive out-of-range case above `i64::MAX`, which parses to
/// `u64` and so is still catchable). Undetectable and unfixable at this
/// layer without `serde_json`'s `arbitrary_precision` feature; documented
/// here as a known v0 gap rather than silently accepted as correct.
pub fn json_to_props(v: &Value) -> Result<Props, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "expected a JSON object for props".to_string())?;
    let mut props = Props::new();
    for (k, val) in obj {
        props.insert(k.clone(), json_to_prop_value(val)?);
    }
    Ok(props)
}

/// A JSON object of property changes for [`topodb::Op::SetNodeProps`]. A `null`
/// value REMOVES the key (`None`); any other JSON scalar SETS it
/// (`Some(PropValue)`, via [`json_to_prop_value`]). `Err` if `v` isn't a JSON
/// object, or a non-null value isn't a representable scalar. The `null`-removes
/// convention is what lets a caller delete a prop over the wire — plain
/// [`json_to_props`] has no way to express removal.
pub fn json_to_prop_changes(v: &Value) -> Result<BTreeMap<String, Option<PropValue>>, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "expected a JSON object for props".to_string())?;
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        let entry = match val {
            Value::Null => None,
            other => Some(json_to_prop_value(other)?),
        };
        out.insert(k.clone(), entry);
    }
    Ok(out)
}

/// A JSON array of finite numbers → `Vec<f32>`, for raw embeddings
/// ([`topodb::Op::SetEmbedding`]) and vector-search queries. `Err` if `v` isn't
/// a JSON array, or any element isn't a finite number. (The host computes
/// embeddings; TopoDB stores/searches the raw floats.)
pub fn json_to_f32_vec(v: &Value) -> Result<Vec<f32>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| "expected a JSON array of numbers".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, el) in arr.iter().enumerate() {
        let f = el
            .as_f64()
            .ok_or_else(|| format!("vector element {i} is not a number: {el}"))?;
        let f = f as f32;
        if !f.is_finite() {
            return Err(format!("vector element {i} is not finite"));
        }
        out.push(f);
    }
    Ok(out)
}

/// Builds the `Props` map for a write tool that has one required, caller-named
/// field (`create_memory`'s `content`, `create_entity`'s `name`) plus an
/// optional JSON `props` object of additional metadata. `key`/`value` are the
/// required field, already converted to a `PropValue`; `extra` is the tool
/// call's optional `props` param, converted via `json_to_props`.
///
/// `Err` if `extra` (once converted) already contains `key` — a collision
/// with the required field is a caller error to be corrected, never silently
/// overwritten. `Err` also propagates straight through from `json_to_props`
/// (non-object `extra`, or an unrepresentable value inside it).
pub fn merge_required_prop(
    key: &str,
    value: PropValue,
    extra: Option<&Value>,
) -> Result<Props, String> {
    let mut props = match extra {
        Some(v) => json_to_props(v)?,
        None => Props::new(),
    };
    if props.contains_key(key) {
        return Err(format!(
            "props must not include {key:?}: it is already set from the tool's own parameter"
        ));
    }
    props.insert(key.to_string(), value);
    Ok(props)
}

/// A `Scope` → its JSON rendering: `"shared"` or the scope's ULID string.
/// Mirrors the `shared`/ULID label convention used across TopoDB's JSON-facing
/// front ends (e.g. `topodb-mcp`'s `db_info` tool).
pub fn scope_to_json(scope: Scope) -> Value {
    Value::String(match scope {
        Scope::Shared => "shared".to_string(),
        Scope::Id(id) => id.to_string(),
    })
}

/// A `NodeRecord` → JSON: `id`/`label` as strings (ULID via `Display` for
/// `id`), `scope` per [`scope_to_json`], and `props` per [`props_to_json`].
/// Deliberately omits the `embedding` field — no MCP v0 tool surfaces vector
/// data (that's a later concern via dedicated embedding tools).
pub fn node_to_json(n: &NodeRecord) -> Result<Value, String> {
    let mut map = Map::new();
    map.insert("id".into(), Value::String(n.id.to_string()));
    map.insert("scope".into(), scope_to_json(n.scope));
    map.insert("label".into(), Value::String(n.label.to_string()));
    map.insert("props".into(), props_to_json(&n.props)?);
    Ok(Value::Object(map))
}

/// An `EdgeRecord` → JSON: `id`/`from`/`to` as ULID strings, `type` for `ty`
/// (JSON-friendlier than the Rust keyword-adjacent field name), `scope` per
/// [`scope_to_json`], `props` per [`props_to_json`], and the temporal bounds
/// `valid_from`/`valid_to` (`valid_to` is `null` while the edge is open).
pub fn edge_to_json(e: &EdgeRecord) -> Result<Value, String> {
    let mut map = Map::new();
    map.insert("id".into(), Value::String(e.id.to_string()));
    map.insert("scope".into(), scope_to_json(e.scope));
    map.insert("type".into(), Value::String(e.ty.to_string()));
    map.insert("from".into(), Value::String(e.from.to_string()));
    map.insert("to".into(), Value::String(e.to.to_string()));
    map.insert("props".into(), props_to_json(&e.props)?);
    map.insert("valid_from".into(), Value::Number(e.valid_from.into()));
    map.insert(
        "valid_to".into(),
        match e.valid_to {
            Some(t) => Value::Number(t.into()),
            None => Value::Null,
        },
    );
    Ok(Value::Object(map))
}

/// A `Subgraph` → `{"nodes": [...], "edges": [...]}`, each element per
/// [`node_to_json`]/[`edge_to_json`].
pub fn subgraph_to_json(sg: &Subgraph) -> Result<Value, String> {
    let nodes: Vec<Value> = sg
        .nodes
        .iter()
        .map(node_to_json)
        .collect::<Result<_, _>>()?;
    let edges: Vec<Value> = sg
        .edges
        .iter()
        .map(edge_to_json)
        .collect::<Result<_, _>>()?;
    Ok(serde_json::json!({ "nodes": nodes, "edges": edges }))
}

/// Resolves a tool's optional `scope` string param to a `Scope`: `None` →
/// `default` (the server's configured default scope); `Some("shared")`
/// (case-insensitive) → `Scope::Shared`; `Some(<ulid>)` → `Scope::Id`; any
/// other string → a clear `Err`. Mirrors `topodb-mcp`'s `config::parse_scope`
/// "shared" / ULID contract, generalized to the `Option` (tool-call) case.
pub fn resolve_scope(scope: Option<&str>, default: Scope) -> Result<Scope, String> {
    match scope {
        None => Ok(default),
        Some(s) if s.eq_ignore_ascii_case("shared") => Ok(Scope::Shared),
        Some(s) => ScopeId::from_str(s)
            .map(Scope::Id)
            .map_err(|e| format!("invalid scope {s:?} (expected \"shared\" or a ULID): {e}")),
    }
}

/// A resolved `Scope` → the singleton `ScopeSet` a read call needs: `Shared`
/// admits only the shared scope, `Id(id)` admits only that one scope id.
pub fn scope_to_scope_set(scope: Scope) -> ScopeSet {
    match scope {
        Scope::Shared => ScopeSet::default().with_shared(),
        Scope::Id(id) => ScopeSet::of(&[id]),
    }
}

/// Several resolved `Scope`s → the `ScopeSet` a multi-scope read runs against.
/// `Scope::Shared` sets the set's `include_shared` flag; each `Scope::Id`
/// becomes a member id. This is the only constructor that can produce a
/// genuinely multi-member `ScopeSet` — [`scope_to_scope_set`] always collapses
/// to a singleton, which is why "this project *plus* shared" was previously
/// unexpressible from any client.
///
/// An empty slice yields a set that admits nothing. Callers must not hand a
/// read an empty set expecting "everything" — there is no unscoped read.
pub fn scopes_to_scope_set(scopes: &[Scope]) -> ScopeSet {
    let ids: Vec<ScopeId> = scopes
        .iter()
        .filter_map(|s| match s {
            Scope::Id(id) => Some(*id),
            Scope::Shared => None,
        })
        .collect();
    let set = ScopeSet::of(&ids);
    if scopes.iter().any(|s| matches!(s, Scope::Shared)) {
        set.with_shared()
    } else {
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topodb::NodeId;

    fn props(pairs: &[(&str, PropValue)]) -> Props {
        pairs
            .iter()
            .cloned()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    // --- PropValue <-> Value: Str/Int/Bool both ways ---

    #[test]
    fn str_round_trips() {
        let v = PropValue::Str("hello".into());
        let j = prop_value_to_json(&v).unwrap();
        assert_eq!(j, Value::String("hello".into()));
        assert_eq!(json_to_prop_value(&j).unwrap(), v);
    }

    #[test]
    fn int_round_trips() {
        let v = PropValue::Int(-42);
        let j = prop_value_to_json(&v).unwrap();
        assert_eq!(j, serde_json::json!(-42));
        assert_eq!(json_to_prop_value(&j).unwrap(), v);
    }

    #[test]
    fn bool_round_trips() {
        for b in [true, false] {
            let v = PropValue::Bool(b);
            let j = prop_value_to_json(&v).unwrap();
            assert_eq!(j, Value::Bool(b));
            assert_eq!(json_to_prop_value(&j).unwrap(), v);
        }
    }

    // --- Float <-> JSON number: JSON int -> Int, JSON float -> Float ---

    #[test]
    fn float_to_json_is_a_json_number() {
        let v = PropValue::Float(3.5);
        let j = prop_value_to_json(&v).unwrap();
        assert_eq!(j, serde_json::json!(3.5));
    }

    #[test]
    fn json_integer_literal_decodes_to_int_not_float() {
        let j = serde_json::json!(7);
        assert_eq!(json_to_prop_value(&j).unwrap(), PropValue::Int(7));
    }

    #[test]
    fn json_float_literal_decodes_to_float() {
        let j = serde_json::json!(7.5);
        assert_eq!(json_to_prop_value(&j).unwrap(), PropValue::Float(7.5));
    }

    #[test]
    fn i64_max_round_trips_as_int() {
        let v = PropValue::Int(i64::MAX);
        let j = prop_value_to_json(&v).unwrap();
        assert_eq!(j, serde_json::json!(i64::MAX));
        assert_eq!(json_to_prop_value(&j).unwrap(), v);
    }

    #[test]
    fn json_integer_above_i64_max_is_an_error_not_a_lossy_float() {
        let j = serde_json::json!(u64::MAX);
        let err = json_to_prop_value(&j).unwrap_err();
        assert!(
            err.contains("integer out of supported range"),
            "expected a clear out-of-range error, got: {err}"
        );
        // And just past the i64 boundary too, not only at the extreme.
        let j = serde_json::json!(i64::MAX as u64 + 1);
        assert!(json_to_prop_value(&j).is_err());
    }

    #[test]
    fn non_finite_float_to_json_is_an_error() {
        assert!(prop_value_to_json(&PropValue::Float(f64::NAN)).is_err());
        assert!(prop_value_to_json(&PropValue::Float(f64::INFINITY)).is_err());
    }

    // --- Bytes/DateTime unsupported, both directions ---

    #[test]
    fn bytes_to_json_is_unsupported() {
        let err = prop_value_to_json(&PropValue::Bytes(vec![1, 2, 3])).unwrap_err();
        assert_eq!(err, UNSUPPORTED);
    }

    #[test]
    fn datetime_to_json_is_unsupported() {
        let err = prop_value_to_json(&PropValue::DateTime(123)).unwrap_err();
        assert_eq!(err, UNSUPPORTED);
    }

    #[test]
    fn json_array_to_propvalue_is_unsupported() {
        let err = json_to_prop_value(&serde_json::json!([1, 2])).unwrap_err();
        assert_eq!(err, UNSUPPORTED);
    }

    #[test]
    fn json_object_to_propvalue_is_unsupported() {
        let err = json_to_prop_value(&serde_json::json!({"a": 1})).unwrap_err();
        assert_eq!(err, UNSUPPORTED);
    }

    #[test]
    fn json_null_to_propvalue_is_unsupported() {
        let err = json_to_prop_value(&Value::Null).unwrap_err();
        assert_eq!(err, UNSUPPORTED);
    }

    // --- Props <-> JSON object ---

    #[test]
    fn props_round_trip() {
        let p = props(&[
            ("name", PropValue::Str("ada".into())),
            ("age", PropValue::Int(30)),
            ("active", PropValue::Bool(true)),
            ("score", PropValue::Float(1.5)),
        ]);
        let j = props_to_json(&p).unwrap();
        assert!(j.is_object());
        let back = json_to_props(&j).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn props_to_json_propagates_unsupported_value() {
        let p = props(&[("blob", PropValue::Bytes(vec![9]))]);
        assert!(props_to_json(&p).is_err());
    }

    #[test]
    fn json_to_props_rejects_non_object() {
        assert!(json_to_props(&serde_json::json!([1, 2])).is_err());
    }

    #[test]
    fn json_to_props_propagates_unsupported_field() {
        let j = serde_json::json!({"bad": [1, 2]});
        assert!(json_to_props(&j).is_err());
    }

    // --- merge_required_prop: the create_memory/create_entity collision rule ---

    #[test]
    fn merge_required_prop_with_no_extra_just_sets_the_key() {
        let props = merge_required_prop("content", PropValue::Str("hi".into()), None).unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(props["content"], PropValue::Str("hi".into()));
    }

    #[test]
    fn merge_required_prop_merges_additional_fields() {
        let extra = serde_json::json!({"source": "chat", "confidence": 3});
        let props =
            merge_required_prop("content", PropValue::Str("hi".into()), Some(&extra)).unwrap();
        assert_eq!(props.len(), 3);
        assert_eq!(props["content"], PropValue::Str("hi".into()));
        assert_eq!(props["source"], PropValue::Str("chat".into()));
        assert_eq!(props["confidence"], PropValue::Int(3));
    }

    #[test]
    fn merge_required_prop_rejects_collision_with_required_key() {
        let extra = serde_json::json!({"content": "sneaky overwrite"});
        let err =
            merge_required_prop("content", PropValue::Str("hi".into()), Some(&extra)).unwrap_err();
        assert!(
            err.contains("content"),
            "error should name the colliding key: {err}"
        );
        // And the same for `name` (create_entity's required key), to confirm
        // this isn't hardcoded to "content".
        let extra = serde_json::json!({"name": "sneaky"});
        assert!(merge_required_prop("name", PropValue::Str("ada".into()), Some(&extra)).is_err());
    }

    #[test]
    fn merge_required_prop_does_not_overwrite_on_collision() {
        // The collision must be rejected outright, not silently resolved by
        // either value winning — assert no props map is returned at all.
        let extra = serde_json::json!({"content": "other"});
        let result = merge_required_prop("content", PropValue::Str("mine".into()), Some(&extra));
        assert!(result.is_err());
    }

    #[test]
    fn merge_required_prop_propagates_non_object_extra() {
        let extra = serde_json::json!([1, 2]);
        assert!(merge_required_prop("content", PropValue::Str("hi".into()), Some(&extra)).is_err());
    }

    // --- node/edge/subgraph -> JSON ---

    fn sample_node(scope: Scope) -> NodeRecord {
        NodeRecord {
            id: NodeId::new(),
            scope,
            label: "Entity".into(),
            props: props(&[("name", PropValue::Str("ada".into()))]),
            embedding: None,
        }
    }

    fn sample_edge(scope: Scope, from: NodeId, to: NodeId) -> EdgeRecord {
        EdgeRecord {
            id: topodb::EdgeId::new(),
            scope,
            ty: "ABOUT".into(),
            from,
            to,
            props: Props::new(),
            valid_from: 1_000,
            valid_to: None,
        }
    }

    #[test]
    fn node_to_json_has_ulid_id_and_declared_fields() {
        let scope = Scope::Id(ScopeId::new());
        let n = sample_node(scope);
        let j = node_to_json(&n).unwrap();
        assert_eq!(j["id"], Value::String(n.id.to_string()));
        assert_eq!(j["label"], Value::String("Entity".into()));
        assert_eq!(j["scope"], scope_to_json(scope));
        assert_eq!(j["props"]["name"], Value::String("ada".into()));
        // `id` round-trips through NodeId's ULID Display/FromStr.
        let parsed: NodeId = j["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(parsed, n.id);
    }

    #[test]
    fn node_to_json_propagates_unsupported_prop() {
        let mut n = sample_node(Scope::Shared);
        n.props.insert("blob".into(), PropValue::Bytes(vec![1]));
        assert!(node_to_json(&n).is_err());
    }

    #[test]
    fn edge_to_json_has_ulid_ids_and_temporal_bounds() {
        let scope = Scope::Shared;
        let a = NodeId::new();
        let b = NodeId::new();
        let e = sample_edge(scope, a, b);
        let j = edge_to_json(&e).unwrap();
        assert_eq!(j["id"], Value::String(e.id.to_string()));
        assert_eq!(j["from"], Value::String(a.to_string()));
        assert_eq!(j["to"], Value::String(b.to_string()));
        assert_eq!(j["type"], Value::String("ABOUT".into()));
        assert_eq!(j["valid_from"], serde_json::json!(1_000));
        assert_eq!(j["valid_to"], Value::Null);
    }

    #[test]
    fn edge_to_json_closed_edge_has_numeric_valid_to() {
        let mut e = sample_edge(Scope::Shared, NodeId::new(), NodeId::new());
        e.valid_to = Some(2_000);
        let j = edge_to_json(&e).unwrap();
        assert_eq!(j["valid_to"], serde_json::json!(2_000));
    }

    #[test]
    fn subgraph_to_json_nests_nodes_and_edges() {
        let scope = Scope::Shared;
        let a = sample_node(scope);
        let b = sample_node(scope);
        let e = sample_edge(scope, a.id, b.id);
        let sg = Subgraph {
            nodes: vec![a.clone(), b.clone()],
            edges: vec![e.clone()],
        };
        let j = subgraph_to_json(&sg).unwrap();
        assert_eq!(j["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(j["edges"].as_array().unwrap().len(), 1);
        assert_eq!(j["edges"][0]["id"], Value::String(e.id.to_string()));
    }

    // --- normalize_edge_type: one relation, one vocabulary entry ---

    #[test]
    fn edge_type_variants_normalize_to_one_form() {
        for raw in [
            "works_at",
            "Works At",
            "works-at",
            "WORKS_AT",
            " works  at ",
            "works--at",
            "works_-at",
        ] {
            assert_eq!(
                normalize_edge_type(raw).unwrap(),
                "works_at",
                "{raw:?} should normalize to works_at"
            );
        }
        assert_eq!(normalize_edge_type("about").unwrap(), "about");
    }

    #[test]
    fn edge_type_empty_after_normalization_is_an_error() {
        for raw in ["", "   ", "---", "_", " - _ "] {
            assert!(normalize_edge_type(raw).is_err(), "{raw:?} should error");
        }
    }

    // --- upgraded_spec: stock specs upgrade, customized specs don't ---

    #[test]
    fn legacy_stock_spec_upgrades_to_current_default() {
        let legacy = IndexSpec {
            equality: vec![PropIndex {
                label: ENTITY_LABEL.into(),
                prop: ENTITY_NAME_PROP.into(),
            }],
            text: vec![PropIndex {
                label: MEMORY_LABEL.into(),
                prop: MEMORY_CONTENT_PROP.into(),
            }],
        };
        assert_eq!(upgraded_spec(legacy), default_spec());
        // Idempotent: the current default maps to itself... via the
        // not-legacy branch (it is not byte-equal to the legacy spec).
        assert_eq!(upgraded_spec(default_spec()), default_spec());
    }

    #[test]
    fn customized_spec_is_never_rewritten() {
        let custom = IndexSpec {
            equality: vec![PropIndex {
                label: "Person".into(),
                prop: "handle".into(),
            }],
            text: vec![PropIndex {
                label: MEMORY_LABEL.into(),
                prop: MEMORY_CONTENT_PROP.into(),
            }],
        };
        assert_eq!(upgraded_spec(custom.clone()), custom);
    }

    // --- scope resolution ---

    #[test]
    fn resolve_scope_none_uses_default() {
        let id = ScopeId::new();
        assert_eq!(resolve_scope(None, Scope::Shared).unwrap(), Scope::Shared);
        assert_eq!(resolve_scope(None, Scope::Id(id)).unwrap(), Scope::Id(id));
    }

    #[test]
    fn resolve_scope_shared_is_case_insensitive() {
        assert_eq!(
            resolve_scope(Some("shared"), Scope::Id(ScopeId::new())).unwrap(),
            Scope::Shared
        );
        assert_eq!(
            resolve_scope(Some("SHARED"), Scope::Id(ScopeId::new())).unwrap(),
            Scope::Shared
        );
    }

    #[test]
    fn resolve_scope_ulid_parses_to_id() {
        let id = ScopeId::new();
        let s = id.to_string();
        assert_eq!(
            resolve_scope(Some(&s), Scope::Shared).unwrap(),
            Scope::Id(id)
        );
    }

    #[test]
    fn resolve_scope_garbage_is_a_clear_error() {
        let err = resolve_scope(Some("not-a-ulid"), Scope::Shared).unwrap_err();
        assert!(err.contains("not-a-ulid"));
    }

    #[test]
    fn scope_to_scope_set_shared_admits_only_shared() {
        let set = scope_to_scope_set(Scope::Shared);
        assert!(set.contains(Scope::Shared));
        assert!(!set.contains(Scope::Id(ScopeId::new())));
    }

    #[test]
    fn scope_to_scope_set_id_admits_only_that_id() {
        let id = ScopeId::new();
        let set = scope_to_scope_set(Scope::Id(id));
        assert!(set.contains(Scope::Id(id)));
        assert!(!set.contains(Scope::Shared));
        assert!(!set.contains(Scope::Id(ScopeId::new())));
    }

    #[test]
    fn scopes_to_scope_set_admits_every_member() {
        let a = ScopeId::new();
        let b = ScopeId::new();
        let set = scopes_to_scope_set(&[Scope::Id(a), Scope::Shared, Scope::Id(b)]);
        assert!(set.contains(Scope::Id(a)));
        assert!(set.contains(Scope::Id(b)));
        assert!(set.contains(Scope::Shared));
    }

    #[test]
    fn scopes_to_scope_set_without_shared_excludes_shared() {
        let a = ScopeId::new();
        let set = scopes_to_scope_set(&[Scope::Id(a)]);
        assert!(set.contains(Scope::Id(a)));
        assert!(!set.contains(Scope::Shared));
    }

    #[test]
    fn scopes_to_scope_set_matches_singleton_for_one_member() {
        // The new multi-member constructor must agree with the existing
        // single-scope one for a one-element input — that equivalence is what
        // makes seeding the server's default read set from a 1-length list
        // backwards compatible.
        let a = ScopeId::new();
        let multi = scopes_to_scope_set(&[Scope::Id(a)]);
        let single = scope_to_scope_set(Scope::Id(a));
        assert_eq!(multi.contains(Scope::Id(a)), single.contains(Scope::Id(a)));
        assert_eq!(
            multi.contains(Scope::Shared),
            single.contains(Scope::Shared)
        );

        let multi_shared = scopes_to_scope_set(&[Scope::Shared]);
        let single_shared = scope_to_scope_set(Scope::Shared);
        assert_eq!(
            multi_shared.contains(Scope::Shared),
            single_shared.contains(Scope::Shared)
        );
    }

    #[test]
    fn scopes_to_scope_set_empty_admits_nothing() {
        let a = ScopeId::new();
        let set = scopes_to_scope_set(&[]);
        assert!(!set.contains(Scope::Shared));
        assert!(!set.contains(Scope::Id(a)));
    }

    // --- json_to_prop_changes: null removes, scalar sets ---

    #[test]
    fn prop_changes_null_is_remove_scalar_is_set() {
        let j = serde_json::json!({ "status": "active", "stale": null, "n": 3 });
        let changes = json_to_prop_changes(&j).unwrap();
        assert_eq!(changes["status"], Some(PropValue::Str("active".into())));
        assert_eq!(changes["stale"], None);
        assert_eq!(changes["n"], Some(PropValue::Int(3)));
    }

    #[test]
    fn prop_changes_rejects_non_object() {
        assert!(json_to_prop_changes(&serde_json::json!([1, 2])).is_err());
    }

    #[test]
    fn prop_changes_propagates_unsupported_value() {
        // A nested array is not a representable scalar (and is not null).
        assert!(json_to_prop_changes(&serde_json::json!({ "x": [1, 2] })).is_err());
    }

    // --- json_to_f32_vec ---

    #[test]
    fn f32_vec_parses_numbers() {
        let j = serde_json::json!([0.0, 1.5, -2, 3]);
        assert_eq!(json_to_f32_vec(&j).unwrap(), vec![0.0f32, 1.5, -2.0, 3.0]);
    }

    #[test]
    fn f32_vec_rejects_non_array() {
        assert!(json_to_f32_vec(&serde_json::json!({"a": 1})).is_err());
    }

    #[test]
    fn f32_vec_rejects_non_number_element() {
        assert!(json_to_f32_vec(&serde_json::json!([1.0, "x"])).is_err());
    }

    #[test]
    fn f32_vec_rejects_overflow_to_infinity() {
        // Finite as f64 but overflows f32 -> must be rejected, not silently Inf.
        assert!(json_to_f32_vec(&serde_json::json!([1e40])).is_err());
    }

    #[test]
    fn default_spec_covers_alias_and_synonym() {
        let s = default_spec();
        let has = |list: &[PropIndex], l: &str, p: &str| {
            list.iter().any(|pi| pi.label == l && pi.prop == p)
        };
        assert!(has(&s.equality, ALIAS_LABEL, ALIAS_NAME_PROP));
        assert!(has(&s.equality, SYNONYM_LABEL, SYNONYM_TERM_PROP));
        assert!(has(&s.text, ALIAS_LABEL, ALIAS_NAME_PROP));
    }

    #[test]
    fn every_stock_generation_upgrades_to_current_default() {
        // v0 (pre-0.0.9): eq (Entity,name); text (Memory,content).
        let v0 = IndexSpec {
            equality: vec![PropIndex {
                label: ENTITY_LABEL.into(),
                prop: ENTITY_NAME_PROP.into(),
            }],
            text: vec![PropIndex {
                label: MEMORY_LABEL.into(),
                prop: MEMORY_CONTENT_PROP.into(),
            }],
        };
        // v1: v0 + text (Entity,name).
        let v1 = IndexSpec {
            equality: v0.equality.clone(),
            text: vec![
                PropIndex {
                    label: MEMORY_LABEL.into(),
                    prop: MEMORY_CONTENT_PROP.into(),
                },
                PropIndex {
                    label: ENTITY_LABEL.into(),
                    prop: ENTITY_NAME_PROP.into(),
                },
            ],
        };
        assert_eq!(upgraded_spec(v0), default_spec());
        assert_eq!(upgraded_spec(v1), default_spec());
        assert_eq!(upgraded_spec(default_spec()), default_spec());
    }
}
