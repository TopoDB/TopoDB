mod cli;
mod output;

use std::io::Read;
use std::str::FromStr;

use clap::Parser;
use cli::{Cli, Command};
use topodb::{
    Db, Direction, EdgeId, NodeId, Op, PropValue, Scope, TopoError, TraversalQuery, VectorQuery,
};

fn main() {
    let cli = Cli::parse();

    // Resolve the default scope once, up front: "shared" (case-insensitive)
    // -> Scope::Shared, a ULID -> Scope::Id, anything else is a caller error
    // the user can fix -> rejected/2.
    let default_scope = match topodb_json::resolve_scope(Some(&cli.scope), Scope::Shared) {
        Ok(s) => s,
        Err(e) => output::fail("rejected", &e, 2),
    };

    // Open using the file's own persisted index spec — no --spec flag on
    // this CLI. An EXISTING file always inherits its persisted spec exactly
    // (via `open_stored`), so a db another tool (e.g. topodb-mcp) already
    // populated is never reindexed or mis-declared. A brand-new file (no
    // `.redb` at this path yet) is created with the SAME canonical
    // `topodb_json::default_spec()` that topodb-mcp uses when `--spec` is
    // omitted — equality on `(Entity, name)`, text on `(Memory, content)` —
    // rather than the engine's bare `IndexSpec::default()` (which declares
    // nothing). This is what makes a CLI-created db and an MCP-created db
    // byte-identical in their persisted `index_spec`: serving one via the
    // other never reindexes, and both `find` and `search` work out of the box
    // on a fresh CLI db. `Path::exists` is safe here: the CLI is a single,
    // non-concurrent process per invocation, so there's no writer racing it.
    // Resolve any per-command --scope override BEFORE opening the db, so a
    // bad value never leaves an empty file behind — same contract as the
    // global --scope above.
    let write_scope = match &cli.cmd {
        Command::CreateMemory { scope, .. }
        | Command::CreateEntity { scope, .. }
        | Command::Link { scope, .. }
        | Command::Remember { scope, .. } => resolve_cmd_scope(scope.as_deref(), default_scope),
        _ => default_scope,
    };

    let db = if cli.db.exists() {
        // Inherit the persisted spec, but silently upgrade a db still on an
        // older STOCK default to the current one (`topodb_json::upgraded_spec`
        // — e.g. adding the (Entity, name) text index); a customized spec is
        // inherited verbatim. Mirrors topodb-mcp's open path exactly.
        Db::open_stored(&cli.db).and_then(|db| {
            let persisted = db.index_spec();
            let upgraded = topodb_json::upgraded_spec(persisted.clone());
            if upgraded != persisted {
                drop(db);
                Db::open_with(&cli.db, upgraded)
            } else {
                Ok(db)
            }
        })
    } else {
        Db::open_with(&cli.db, topodb_json::default_spec())
    };
    let db = match db {
        Ok(db) => db,
        Err(e) => output::fail_engine(&e),
    };

    match cli.cmd {
        Command::Info => info(&db, &cli.db, default_scope, cli.pretty),
        Command::CreateMemory { content, props, .. } => {
            create_memory(&db, write_scope, content, props.as_deref(), cli.pretty)
        }
        Command::CreateEntity { name, props, .. } => {
            create_entity(&db, write_scope, name, props.as_deref(), cli.pretty)
        }
        Command::Link {
            from,
            to,
            ty,
            props,
            valid_from,
            ..
        } => link(
            &db,
            write_scope,
            &from,
            &to,
            ty,
            props.as_deref(),
            valid_from,
            cli.pretty,
        ),
        Command::Remember {
            content,
            entity,
            edge_type,
            supersedes,
            props,
            ..
        } => remember(
            &db,
            write_scope,
            content,
            entity,
            edge_type,
            supersedes,
            props.as_deref(),
            cli.pretty,
        ),
        Command::Get { id } => get(&db, default_scope, &id, cli.pretty),
        Command::Find {
            label,
            prop,
            value,
            normalized,
        } => find(
            &db,
            default_scope,
            &label,
            &prop,
            &value,
            normalized,
            cli.pretty,
        ),
        Command::Search { query, k } => search(&db, default_scope, &query, k, cli.pretty),
        Command::Traverse {
            seed,
            max_hops,
            direction,
            edge_type,
        } => traverse(
            &db,
            default_scope,
            &seed,
            max_hops,
            direction.into(),
            edge_type,
            cli.pretty,
        ),
        Command::Stats { id } => stats(&db, default_scope, &id, cli.pretty),
        Command::Changes { since } => changes(&db, since, cli.pretty),
        Command::Compact { keep_from } => compact(&db, keep_from, cli.pretty),
        Command::SetProps { id, props } => set_props(&db, &id, &props, cli.pretty),
        Command::RemoveNode { id } => remove_node(&db, &id, cli.pretty),
        Command::CloseEdge { id, valid_to } => close_edge(&db, &id, valid_to, cli.pretty),
        Command::SetEmbedding { id, model, vector } => {
            set_embedding(&db, &id, model, &vector, cli.pretty)
        }
        Command::SearchVector {
            model,
            vector,
            k,
            candidate,
        } => search_vector(&db, default_scope, model, &vector, k, candidate, cli.pretty),
        Command::Submit { input } => submit(&db, default_scope, &input, cli.pretty),
    }
}

/// Resolves a per-command `--scope` override against the global `--scope`.
/// Absent → the global default; present → parsed, a bad value being a
/// caller-fixable input error (rejected/exit-2). Routed through the same
/// `topodb_json::resolve_scope` the batch DSL uses, so `topodb link --scope X`
/// and `topodb submit '[{"op":"link", ..., "scope":"X"}]'` cannot drift apart.
fn resolve_cmd_scope(scope: Option<&str>, default: Scope) -> Scope {
    match topodb_json::resolve_scope(scope, default) {
        Ok(s) => s,
        Err(e) => output::fail("rejected", &e, 2),
    }
}

/// Parses a `--value` arg per the CLI's find semantics: try it as a JSON
/// scalar first (so `42` -> `Int(42)`, `true` -> `Bool(true)`,
/// `"ada"` -> `Str("ada")`); if it doesn't parse as JSON at all, fall back to
/// treating the raw string as `PropValue::Str` (so `--value ada` and
/// `--value '"ada"'` are equivalent). A JSON value that parses but isn't a
/// scalar `json_to_prop_value` can represent (array/object/null) is a
/// caller-fixable input error -> `fail("rejected", .., 2)`.
fn parse_value_arg(value: &str) -> PropValue {
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(v) => match topodb_json::json_to_prop_value(&v) {
            Ok(pv) => pv,
            Err(e) => output::fail("rejected", &format!("parsing --value: {e}"), 2),
        },
        Err(_) => PropValue::Str(value.to_string()),
    }
}

fn get(db: &Db, scope: Scope, id: &str, pretty: bool) -> ! {
    let id = match NodeId::from_str(id) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid id {id:?}: {e}"), 2),
    };
    let scopes = topodb_json::scope_to_scope_set(scope);
    let value = match db.node(&scopes, id) {
        Some(n) => {
            let node = match topodb_json::node_to_json(&n) {
                Ok(v) => v,
                Err(e) => output::fail("internal", &e, 1),
            };
            serde_json::json!({ "found": true, "node": node })
        }
        None => serde_json::json!({ "found": false }),
    };
    output::ok(&value, pretty);
}

fn find(
    db: &Db,
    scope: Scope,
    label: &str,
    prop: &str,
    value: &str,
    normalized: bool,
    pretty: bool,
) -> ! {
    let pv = parse_value_arg(value);
    let scopes = topodb_json::scope_to_scope_set(scope);
    let hits = if normalized {
        db.nodes_by_prop_normalized(&scopes, label, prop, &pv)
    } else {
        db.nodes_by_prop(&scopes, label, prop, &pv)
    };
    let hits = match hits {
        Ok(hits) => hits,
        Err(e) => output::fail_engine(&e),
    };
    let nodes: Vec<serde_json::Value> = match hits.iter().map(topodb_json::node_to_json).collect() {
        Ok(nodes) => nodes,
        Err(e) => output::fail("internal", &e, 1),
    };
    output::ok(&serde_json::Value::Array(nodes), pretty);
}

fn search(db: &Db, scope: Scope, query: &str, k: usize, pretty: bool) -> ! {
    let scopes = topodb_json::scope_to_scope_set(scope);
    let hits = match db.search_text(&scopes, query, k) {
        Ok(hits) => hits,
        Err(e) => output::fail_engine(&e),
    };
    let out: Result<Vec<serde_json::Value>, String> = hits
        .iter()
        .map(|(n, score)| {
            topodb_json::node_to_json(n)
                .map(|node| serde_json::json!({ "node": node, "score": score }))
        })
        .collect();
    let out = match out {
        Ok(out) => out,
        Err(e) => output::fail("internal", &e, 1),
    };
    output::ok(&serde_json::Value::Array(out), pretty);
}

#[allow(clippy::too_many_arguments)]
fn traverse(
    db: &Db,
    scope: Scope,
    seed: &str,
    max_hops: u8,
    direction: Direction,
    edge_type: Vec<String>,
    pretty: bool,
) -> ! {
    let seed = match NodeId::from_str(seed) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid seed id {seed:?}: {e}"), 2),
    };
    let scopes = topodb_json::scope_to_scope_set(scope);
    // Empty --edge-type (none given) -> None, follow every edge type; the
    // engine treats `Some(vec![])` as "match nothing", which would silently
    // strand the traversal at the seed — so an empty CLI list must map to
    // `None`, not `Some(vec![])`.
    let edge_types = if edge_type.is_empty() {
        None
    } else {
        Some(edge_type.into_iter().map(Into::into).collect())
    };
    let query = TraversalQuery {
        scopes,
        seeds: vec![seed],
        max_hops,
        edge_types,
        direction,
        as_of: None,
    };
    let sg = match db.traverse(&query) {
        Ok(sg) => sg,
        Err(e) => output::fail_engine(&e),
    };
    let subgraph = match topodb_json::subgraph_to_json(&sg) {
        Ok(v) => v,
        Err(e) => output::fail("internal", &e, 1),
    };
    output::ok(&serde_json::json!({ "subgraph": subgraph }), pretty);
}

fn stats(db: &Db, scope: Scope, id: &str, pretty: bool) -> ! {
    let id = match NodeId::from_str(id) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid id {id:?}: {e}"), 2),
    };
    let scopes = topodb_json::scope_to_scope_set(scope);
    let value = match db.access_stats(&scopes, id) {
        Ok(Some(s)) => serde_json::json!({
            "found": true,
            "access_stats": {
                "access_count": s.access_count,
                "last_accessed_at": s.last_accessed_at,
            }
        }),
        Ok(None) => serde_json::json!({ "found": false }),
        Err(e) => output::fail_engine(&e),
    };
    output::ok(&value, pretty);
}

fn changes(db: &Db, since: u64, pretty: bool) -> ! {
    let events = match db.ops_since(since) {
        Ok(events) => events,
        // `Compacted` (the requested range is below the retained floor) is a
        // caller-fixable condition — the caller re-anchors from current
        // state — so it routes to rejected/exit-2, not fail_engine's
        // internal/exit-1 default for non-Rejected variants. Every other
        // error (Storage, Closed, ...) is a genuine internal failure.
        Err(e @ TopoError::Compacted { .. }) => output::fail("rejected", &e.to_string(), 2),
        Err(e) => output::fail_engine(&e),
    };
    let out: Vec<serde_json::Value> = events
        .into_iter()
        .map(|ev| serde_json::json!({ "seq": ev.seq, "op": serde_json::to_value(&*ev.op).unwrap_or(serde_json::Value::Null) }))
        .collect();
    output::ok(&serde_json::Value::Array(out), pretty);
}

fn compact(db: &Db, keep_from: u64, pretty: bool) -> ! {
    if let Err(e) = db.compact_ops(keep_from) {
        output::fail_engine(&e);
    }
    output::ok(&serde_json::json!({ "oldest": keep_from }), pretty);
}

fn info(db: &Db, path: &std::path::Path, default_scope: Scope, pretty: bool) -> ! {
    let current_seq = match db.current_seq() {
        Ok(seq) => seq,
        Err(e) => output::fail_engine(&e),
    };
    let value = serde_json::json!({
        "path": path.to_string_lossy(),
        "format_version": db.format_version(),
        "current_seq": current_seq,
        "index_spec": db.index_spec(),
        "default_scope": topodb_json::scope_to_json(default_scope),
    });
    output::ok(&value, pretty);
}

/// Parses an optional `--props` JSON-object-string arg into a `Value`, for
/// handing to `merge_required_prop`/`json_to_props`. A malformed JSON string
/// is a caller-fixable input error -> `fail("rejected", .., 2)`, matching the
/// exit-code contract for bad input (never a panic).
fn parse_props_arg(props: Option<&str>) -> Option<serde_json::Value> {
    props.map(|s| match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &format!("parsing --props as JSON: {e}"), 2),
    })
}

fn create_memory(db: &Db, scope: Scope, content: String, props: Option<&str>, pretty: bool) -> ! {
    let extra = parse_props_arg(props);
    let props = match topodb_json::merge_required_prop(
        topodb_json::MEMORY_CONTENT_PROP,
        PropValue::Str(content),
        extra.as_ref(),
    ) {
        Ok(p) => p,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let id = NodeId::new();
    let op = Op::CreateNode {
        id,
        scope,
        label: topodb_json::MEMORY_LABEL.into(),
        props,
    };
    if let Err(e) = db.submit(vec![op]) {
        output::fail_engine(&e);
    }
    output::ok(&serde_json::json!({ "id": id.to_string() }), pretty);
}

fn create_entity(db: &Db, scope: Scope, name: String, props: Option<&str>, pretty: bool) -> ! {
    let extra = parse_props_arg(props);
    let props = match topodb_json::merge_required_prop(
        topodb_json::ENTITY_NAME_PROP,
        PropValue::Str(name),
        extra.as_ref(),
    ) {
        Ok(p) => p,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let id = NodeId::new();
    let op = Op::CreateNode {
        id,
        scope,
        label: topodb_json::ENTITY_LABEL.into(),
        props,
    };
    if let Err(e) = db.submit(vec![op]) {
        output::fail_engine(&e);
    }
    output::ok(&serde_json::json!({ "id": id.to_string() }), pretty);
}

#[allow(clippy::too_many_arguments)]
fn link(
    db: &Db,
    scope: Scope,
    from: &str,
    to: &str,
    ty: String,
    props: Option<&str>,
    valid_from: Option<i64>,
    pretty: bool,
) -> ! {
    let from = match NodeId::from_str(from) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid --from id {from:?}: {e}"), 2),
    };
    let to = match NodeId::from_str(to) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid --to id {to:?}: {e}"), 2),
    };
    let props = match parse_props_arg(props) {
        Some(v) => match topodb_json::json_to_props(&v) {
            Ok(p) => p,
            Err(e) => output::fail("rejected", &e, 2),
        },
        None => topodb::Props::new(),
    };
    // Same edge-type vocabulary normalization as the MCP `link` tool and the
    // batch DSL — the three write paths must not fragment the type dict.
    let ty = match topodb_json::normalize_edge_type(&ty) {
        Ok(t) => t,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let id = EdgeId::new();
    let op = Op::CreateEdge {
        id,
        scope,
        ty: ty.into(),
        from,
        to,
        props,
        valid_from,
    };
    if let Err(e) = db.submit(vec![op]) {
        output::fail_engine(&e);
    }
    output::ok(&serde_json::json!({ "id": id.to_string() }), pretty);
}

/// Composed store+link (see the spec): plan via the shared
/// `topodb_json::plan_remember`, submit the one batch, echo the plan.
#[allow(clippy::too_many_arguments)]
fn remember(
    db: &Db,
    scope: Scope,
    content: String,
    entities: Vec<String>,
    edge_type: Option<String>,
    supersedes: Vec<String>,
    props: Option<&str>,
    pretty: bool,
) -> ! {
    let extra = parse_props_arg(props);
    let req = topodb_json::RememberRequest {
        content,
        entities,
        edge_type,
        supersedes,
        props: extra,
    };
    // Collision surface: the write scope plus shared — a shared entity must
    // be found from a project-scoped write, not shadowed by a local twin.
    let lookup = topodb_json::scopes_to_scope_set(&[scope, Scope::Shared]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let plan = match topodb_json::plan_remember(db, scope, &lookup, now, &req) {
        Ok(p) => p,
        Err(topodb_json::ComposeError::Invalid(m)) => output::fail("rejected", &m, 2),
        Err(topodb_json::ComposeError::Engine(e)) => output::fail_engine(&e),
    };
    let topodb_json::RememberPlan {
        ops,
        memory_id,
        deduplicated,
        entities,
        edge_ids,
        superseded,
        ..
    } = plan;
    if !ops.is_empty() {
        if let Err(e) = db.submit(ops) {
            output::fail_engine(&e);
        }
    }
    let entities: Vec<serde_json::Value> = entities
        .into_iter()
        .map(
            |e| serde_json::json!({ "name": e.name, "id": e.id.to_string(), "created": e.created }),
        )
        .collect();
    output::ok(
        &serde_json::json!({
            "memory_id": memory_id.to_string(),
            "deduplicated": deduplicated,
            "entities": entities,
            "edge_ids": edge_ids,
            "superseded": superseded,
        }),
        pretty,
    );
}

fn set_props(db: &Db, id: &str, props: &str, pretty: bool) -> ! {
    let id = match NodeId::from_str(id) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid id {id:?}: {e}"), 2),
    };
    let value: serde_json::Value = match serde_json::from_str(props) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &format!("parsing --props as JSON: {e}"), 2),
    };
    let changes = match topodb_json::json_to_prop_changes(&value) {
        Ok(c) => c,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let applied = match db.submit(vec![Op::SetNodeProps { id, props: changes }]) {
        Ok(a) => a,
        Err(e) => output::fail_engine(&e),
    };
    output::ok(&serde_json::json!({ "seq": applied.last_seq }), pretty);
}

fn remove_node(db: &Db, id: &str, pretty: bool) -> ! {
    let id = match NodeId::from_str(id) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid id {id:?}: {e}"), 2),
    };
    let applied = match db.submit(vec![Op::RemoveNode { id }]) {
        Ok(a) => a,
        Err(e) => output::fail_engine(&e),
    };
    output::ok(&serde_json::json!({ "seq": applied.last_seq }), pretty);
}

fn close_edge(db: &Db, id: &str, valid_to: Option<i64>, pretty: bool) -> ! {
    let id = match EdgeId::from_str(id) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid edge id {id:?}: {e}"), 2),
    };
    let applied = match db.submit(vec![Op::CloseEdge { id, valid_to }]) {
        Ok(a) => a,
        Err(e) => output::fail_engine(&e),
    };
    output::ok(&serde_json::json!({ "seq": applied.last_seq }), pretty);
}

fn set_embedding(db: &Db, id: &str, model: String, vector: &str, pretty: bool) -> ! {
    let id = match NodeId::from_str(id) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid id {id:?}: {e}"), 2),
    };
    let vector_json: serde_json::Value = match serde_json::from_str(vector) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &format!("parsing --vector as JSON: {e}"), 2),
    };
    let vector = match topodb_json::json_to_f32_vec(&vector_json) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let applied = match db.submit(vec![Op::SetEmbedding { id, model, vector }]) {
        Ok(a) => a,
        Err(e) => output::fail_engine(&e),
    };
    output::ok(&serde_json::json!({ "seq": applied.last_seq }), pretty);
}

#[allow(clippy::too_many_arguments)]
fn search_vector(
    db: &Db,
    scope: Scope,
    model: String,
    vector: &str,
    k: usize,
    candidate: Vec<String>,
    pretty: bool,
) -> ! {
    let vector_json: serde_json::Value = match serde_json::from_str(vector) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &format!("parsing --vector as JSON: {e}"), 2),
    };
    let vector = match topodb_json::json_to_f32_vec(&vector_json) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &e, 2),
    };
    // Empty --candidate -> None (score the whole scope); a non-empty list is
    // parsed to NodeIds, any bad id being a caller-fixable rejected/exit-2.
    let candidates = if candidate.is_empty() {
        None
    } else {
        let mut ids = Vec::with_capacity(candidate.len());
        for c in &candidate {
            match NodeId::from_str(c) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    output::fail("rejected", &format!("invalid --candidate id {c:?}: {e}"), 2)
                }
            }
        }
        Some(ids)
    };
    let scopes = topodb_json::scope_to_scope_set(scope);
    let query = VectorQuery {
        scopes,
        model,
        vector,
        k,
        candidates,
    };
    let hits = match db.search_vector(&query) {
        Ok(h) => h,
        Err(e) => output::fail_engine(&e),
    };
    let out: Result<Vec<serde_json::Value>, String> = hits
        .iter()
        .map(|(n, score)| {
            topodb_json::node_to_json(n)
                .map(|node| serde_json::json!({ "node": node, "score": score }))
        })
        .collect();
    let out = match out {
        Ok(out) => out,
        Err(e) => output::fail("internal", &e, 1),
    };
    output::ok(&serde_json::Value::Array(out), pretty);
}

fn submit(db: &Db, default_scope: Scope, input: &str, pretty: bool) -> ! {
    let raw = if input == "-" {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            output::fail("internal", &format!("reading stdin: {e}"), 1);
        }
        buf
    } else {
        match std::fs::read_to_string(input) {
            Ok(s) => s,
            Err(e) => output::fail("rejected", &format!("reading {input:?}: {e}"), 2),
        }
    };
    let batch: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => output::fail("rejected", &format!("parsing batch as JSON: {e}"), 2),
    };
    let (ops, ids) = match topodb_json::resolve_batch(&batch, default_scope) {
        Ok(pair) => pair,
        Err(e) => output::fail("rejected", &e, 2),
    };
    if let Err(e) = db.submit(ops) {
        output::fail_engine(&e);
    }
    let ids: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|o| {
            o.map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();
    output::ok(&serde_json::json!({ "ids": ids }), pretty);
}
