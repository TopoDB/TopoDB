mod cli;
mod output;

use std::str::FromStr;

use clap::Parser;
use cli::{Cli, Command};
use topodb::{Db, EdgeId, NodeId, Op, PropValue, Scope};

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
    // this CLI; a fresh file gets IndexSpec::default().
    let db = match Db::open_stored(&cli.db) {
        Ok(db) => db,
        Err(e) => output::fail_engine(&e),
    };

    match cli.cmd {
        Command::Info => info(&db, &cli.db, default_scope, cli.pretty),
        Command::CreateMemory { content, props } => {
            create_memory(&db, default_scope, content, props.as_deref(), cli.pretty)
        }
        Command::CreateEntity { name, props } => {
            create_entity(&db, default_scope, name, props.as_deref(), cli.pretty)
        }
        Command::Link {
            from,
            to,
            ty,
            props,
            valid_from,
        } => link(
            &db,
            default_scope,
            &from,
            &to,
            ty,
            props.as_deref(),
            valid_from,
            cli.pretty,
        ),
    }
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
