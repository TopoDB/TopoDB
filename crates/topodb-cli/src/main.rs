mod cli;
mod output;
mod resolve;

use std::io::{IsTerminal, Read};
use std::path::Path;
use std::str::FromStr;

use clap::Parser;
use cli::{Cli, Command};
use topodb::{
    Db, Direction, EdgeId, EdgeRecord, NodeId, Op, PropValue, Scope, TimeAxis, TopoError,
    TraversalQuery, ValidInterval, VectorQuery,
};

fn main() {
    let cli = Cli::parse();

    // Project config (nearest .topodb.toml on the path from cwd up).
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let cfg = match resolve::load_project_config(&cwd) {
        Ok(c) => c,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let cfg = cfg.unwrap_or_default();
    for k in &cfg.unknown_keys {
        eprintln!("topodb: ignoring unknown key {k:?} in .topodb.toml");
    }
    let cfg_path = cfg.path.clone();
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok());

    // db path.
    let db_r = resolve::resolve_db(
        cli.db.clone(),
        std::env::var("TOPODB_DB").ok(),
        cfg.db.as_deref().zip(cfg_path.as_deref()),
        home.as_deref(),
    );
    let db_path = db_r.value;

    // Create the parent directory for the default db path (~/.topodb/memory.redb).
    // For user-provided paths, let the database engine report errors if the parent doesn't exist.
    if matches!(db_r.source, resolve::Source::Default) {
        // Guard against unresolved home directory (HOME/USERPROFILE both unset).
        if db_path
            .to_str()
            .map(|p| p.starts_with("~"))
            .unwrap_or(false)
        {
            output::fail(
                "internal",
                "cannot resolve home directory (HOME/USERPROFILE unset) for the default db path",
                1,
            );
        }
        if let Some(parent) = db_path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    output::fail("internal", &format!("creating db directory: {e}"), 1);
                }
            }
        }
    }

    // scope string → Scope. Errors name the source they came from.
    let scope_r = resolve::resolve_scope_str(
        cli.scope.clone(),
        std::env::var("TOPODB_SCOPE").ok(),
        cfg.scope.as_deref().zip(cfg_path.as_deref()),
    );
    let default_scope = match topodb_json::resolve_scope(Some(&scope_r.value), Scope::Shared) {
        Ok(s) => s,
        Err(e) => output::fail(
            "rejected",
            &format!("scope from {}: {e}", scope_r.source.label()),
            2,
        ),
    };

    let fmt = match resolve::resolve_format(
        cli.format.map(Into::into),
        std::env::var("TOPODB_FORMAT").ok(),
        cfg.format.as_deref().zip(cfg_path.as_deref()),
        std::io::stdout().is_terminal(),
    ) {
        Ok(r) => r.value,
        Err(e) => output::fail("rejected", &e, 2),
    };
    let text_mode = matches!(fmt, resolve::Format::Text);
    let scope_display = scope_r.value.clone();
    let scope_source = scope_r.source.label();

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
        | Command::Remember { scope, .. }
        | Command::Forget { scope, .. }
        | Command::ObsidianIngest { scope, .. } => {
            resolve_cmd_scope(scope.as_deref(), default_scope)
        }
        _ => default_scope,
    };

    let db = topodb_json::open_with_busy_retry(cli.lock_wait_ms, || {
        if db_path.exists() {
            // Inherit the persisted spec, but silently upgrade a db still on an
            // older STOCK default to the current one (`topodb_json::upgraded_spec`
            // — e.g. adding the (Entity, name) text index); a customized spec is
            // inherited verbatim. Mirrors topodb-mcp's open path exactly.
            Db::open_stored(&db_path).and_then(|db| {
                let persisted = db.index_spec();
                let upgraded = topodb_json::upgraded_spec(persisted.clone());
                if upgraded != persisted {
                    drop(db);
                    Db::open_with(&db_path, upgraded)
                } else {
                    Ok(db)
                }
            })
        } else {
            Db::open_with(&db_path, topodb_json::default_spec())
        }
    });
    let db = match db {
        Ok(db) => db,
        Err(TopoError::Busy) => output::fail(
            "busy",
            &format!(
                "another process holds {}; retried for {}ms (tune with --lock-wait-ms / TOPODB_LOCK_WAIT_MS)",
                db_path.display(),
                cli.lock_wait_ms
            ),
            3,
        ),
        Err(e) => output::fail_engine(&e),
    };

    match cli.cmd {
        Command::Info => info(&db, &db_path, default_scope, cli.pretty),
        Command::CreateMemory { content, props, .. } => {
            create_memory(&db, write_scope, content, props.as_deref(), cli.pretty)
        }
        Command::CreateEntity {
            name,
            props,
            always_create,
            ..
        } => create_entity(
            &db,
            write_scope,
            name,
            props.as_deref(),
            always_create,
            cli.pretty,
        ),
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
            content_flag,
            entity,
            edge_type,
            supersedes,
            props,
            kind,
            ..
        } => {
            let content = match content.or(content_flag) {
                Some(c) => c,
                None => output::fail(
                    "rejected",
                    "provide the fact as a positional argument or via --content",
                    2,
                ),
            };
            remember(
                &db,
                write_scope,
                content,
                entity,
                edge_type,
                supersedes,
                props.as_deref(),
                kind,
                text_mode,
                cli.pretty,
            )
        }
        Command::Forget { ids, .. } => forget(&db, write_scope, &ids, text_mode, cli.pretty),
        Command::ObsidianIngest { vault, dry_run, .. } => {
            obsidian_ingest(&db, &vault, write_scope, dry_run, cli.pretty)
        }
        Command::ObsidianSeed {
            vault,
            query,
            k,
            entity,
            hops,
            overwrite,
        } => obsidian_seed(
            &db,
            &vault,
            query,
            k,
            entity,
            hops,
            overwrite,
            default_scope,
            cli.pretty,
        ),
        Command::Get { id } => get(&db, default_scope, &id, text_mode, cli.pretty),
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
            text_mode,
            &scope_display,
            &scope_source,
            cli.pretty,
        ),
        Command::Search {
            query,
            k,
            include_superseded,
            kinds,
            recency_weight,
            recency_half_life_days,
            created_after,
            created_before,
            no_temporal_rewrite,
        } => search(
            &db,
            default_scope,
            &query,
            k,
            include_superseded,
            &kinds,
            recency_weight,
            recency_half_life_days,
            created_after,
            created_before,
            no_temporal_rewrite,
            text_mode,
            &scope_display,
            &scope_source,
            cli.pretty,
        ),
        Command::Traverse {
            seed,
            max_hops,
            direction,
            edge_type,
            as_of,
            time_axis,
            valid_interval,
        } => traverse(
            &db,
            default_scope,
            &seed,
            max_hops,
            direction.into(),
            edge_type,
            as_of,
            time_axis.into(),
            allen_interval(&valid_interval),
            cli.pretty,
        ),
        Command::GetEdges {
            from,
            to,
            edge_type,
            open_only,
            as_of,
            direction,
            time_axis,
            valid_interval,
        } => get_edges(
            &db,
            default_scope,
            &from,
            to.as_deref(),
            edge_type.as_deref(),
            open_only,
            as_of,
            direction,
            time_axis.into(),
            allen_interval(&valid_interval),
            cli.pretty,
        ),
        Command::Stats { id } => stats(&db, default_scope, &id, cli.pretty),
        Command::LifecycleCandidates {
            limit,
            half_life_episodic_days,
            half_life_semantic_days,
            half_life_procedural_days,
            half_life_decision_days,
            now_ms,
        } => lifecycle_candidates(
            &db,
            default_scope,
            limit,
            half_life_episodic_days,
            half_life_semantic_days,
            half_life_procedural_days,
            half_life_decision_days,
            now_ms,
            cli.pretty,
        ),
        Command::Purge {
            tombstoned_before,
            yes,
        } => purge(&db, default_scope, tombstoned_before, yes, cli.pretty),
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

/// Resolves the four `--valid-*` flags (the pragmatic allen subset —
/// `topodb::ValidInterval`) into at most one predicate. More than one is a
/// caller-fixable conflict -> `fail("rejected", .., 2)` naming the flags;
/// range flags parse `A..B` into two Unix-ms timestamps. Uses the engine's
/// shared `ValidInterval::from_parts` to handle mutual-exclusion checks, range
/// inversion, and timestamp positivity validation. Timestamp errors from the
/// engine are surfaced verbatim to the CLI exit code contract (exit 2).
fn allen_interval(args: &cli::ValidIntervalArgs) -> Option<ValidInterval> {
    let parse_range = |flag: &str, s: &str| -> Option<(i64, i64)> {
        let parsed = s
            .split_once("..")
            .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)));
        match parsed {
            Some(pair) => Some(pair),
            None => output::fail(
                "rejected",
                &format!("parsing --{flag}: expected A..B with Unix-ms timestamps, got {s:?}"),
                2,
            ),
        }
    };

    let during = args
        .valid_during
        .as_deref()
        .and_then(|s| parse_range("valid-during", s));
    let overlaps = args
        .valid_overlaps
        .as_deref()
        .and_then(|s| parse_range("valid-overlaps", s));
    let before = args.valid_before;
    let after = args.valid_after;

    match ValidInterval::from_parts(during, overlaps, before, after) {
        Ok(iv) => iv,
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

fn get(db: &Db, scope: Scope, id: &str, text_mode: bool, pretty: bool) -> ! {
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
    let text = match value.get("found").and_then(|f| f.as_bool()) {
        Some(true) => value.get("node").map(|n| {
            let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let label = n.get("label").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{id}  {label}")
        }),
        _ => Some("not found".to_string()),
    };
    output::render(&value, text, text_mode, pretty);
}

#[allow(clippy::too_many_arguments)]
fn find(
    db: &Db,
    scope: Scope,
    label: &str,
    prop: &str,
    value: &str,
    normalized: bool,
    text_mode: bool,
    scope_display: &str,
    scope_source: &str,
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
    if nodes.is_empty() {
        output::empty_scope_echo(scope_display, scope_source);
    }
    let text = Some(
        nodes
            .iter()
            .map(|n| {
                let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let label = n.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                format!("{id}  {label}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let value = serde_json::Value::Array(nodes);
    output::render(&value, text, text_mode, pretty);
}

#[allow(clippy::too_many_arguments)]
fn search(
    db: &Db,
    scope: Scope,
    query: &str,
    k: usize,
    include_superseded: bool,
    kinds: &[String],
    recency_weight: f32,
    recency_half_life_days: Option<f64>,
    created_after: Option<String>,
    created_before: Option<String>,
    no_temporal_rewrite: bool,
    text_mode: bool,
    scope_display: &str,
    scope_source: &str,
    pretty: bool,
) -> ! {
    let scopes = topodb_json::scope_to_scope_set(scope);
    // The kinds filter is policy over the engine's generic prop_retain:
    // this layer names the prop and maps "absent" to the default kind.
    let prop_retain = if kinds.is_empty() {
        None
    } else {
        for kind in kinds {
            if let Err(e) = topodb_json::validate_memory_kind(kind) {
                output::fail("rejected", &e, 2);
            }
        }
        Some(topodb::PropRetain {
            prop: topodb_json::MEMORY_KIND_PROP.to_string(),
            any_of: kinds.to_vec(),
            absent_as: Some(topodb_json::MEMORY_KIND_DEFAULT.to_string()),
        })
    };

    // Created-time filter (explicit flags; Task 8 adds the rewriter
    // branch with the same precedence as MCP search_memories). Applied
    // filters are echoed to stderr so operators see what ran.
    let (mut after_ms, mut before_ms) = (None, None);
    let mut filter_note: Option<(String, &str)> = None;
    let mut query = query.to_string();
    if created_after.is_some() || created_before.is_some() {
        let parse = |flag: &str, s: &str| {
            topodb_json::parse_iso_instant(s).unwrap_or_else(|| {
                output::fail(
                    "rejected",
                    &format!(
                        "parsing --{flag}: {s:?} is not an ISO date or UTC datetime \
                         (try 2026-08-01 or 2026-08-01T15:30:00Z)"
                    ),
                    2,
                )
            })
        };
        after_ms = created_after.as_deref().map(|s| parse("created-after", s));
        before_ms = created_before
            .as_deref()
            .map(|s| parse("created-before", s));
        let mut parts = Vec::new();
        if let Some(s) = &created_after {
            parts.push(format!("after {s}"));
        }
        if let Some(s) = &created_before {
            parts.push(format!("before {s}"));
        }
        filter_note = Some((parts.join(" "), "params"));
    } else if !no_temporal_rewrite {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Some(rw) = topodb_json::parse_temporal_query(&query, now) {
            after_ms = rw.after_ms;
            before_ms = rw.before_ms;
            query = rw.residual_query;
            filter_note = Some((rw.matched_phrase, "rewrite"));
        }
    }
    if let Some((desc, source)) = &filter_note {
        output::time_filter_echo(desc, source, after_ms, before_ms);
    }

    let options = topodb::SearchOptions {
        created_range: (after_ms.is_some() || before_ms.is_some()).then_some(
            topodb::CreatedRange {
                after_ms,
                before_ms,
            },
        ),
        prop_retain,
        recency_weight,
        recency_half_life_ms: recency_half_life_days
            .map(|d| (d * 86_400_000.0) as i64)
            .unwrap_or(30 * 24 * 60 * 60 * 1000),
        recency_half_life_by_prop: recency_half_life_days
            .is_none()
            .then(topodb_json::memory_kind_half_life),
        ..topodb::SearchOptions::default()
    };
    // Search is a recall surface: memories retired by `remember --supersedes`
    // are dropped by default (before top-k, unbumped), matching the MCP
    // server's `search_memories`. `--include-superseded` widens the corpus
    // to retired memories too — it does not touch scoring; `--recency-weight
    // 0` is what restores raw BM25. Forgotten memories are also dropped from
    // default search (same liveness model as superseded).
    let hits = if include_superseded {
        db.search_text_with(&scopes, &query, k, &options)
    } else {
        db.search_text_live(
            &scopes,
            &query,
            k,
            &options,
            &topodb_json::MEMORY_TOMBSTONE_PROPS,
        )
    };
    let hits = match hits {
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
    if out.is_empty() {
        output::empty_scope_echo(scope_display, scope_source);
    }
    let text = Some(
        out.iter()
            .map(|hit| {
                let content = hit["node"]["props"]["content"].as_str().unwrap_or("");
                let content = if content.chars().count() > 140 {
                    let mut s: String = content.chars().take(139).collect();
                    s.push('…');
                    s
                } else {
                    content.to_string()
                };
                let id = hit["node"]["id"].as_str().unwrap_or("?");
                let score = hit["score"].as_f64().unwrap_or(0.0);
                format!("- [{score:.2}] {content}  {id}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let value = serde_json::Value::Array(out);
    output::render(&value, text, text_mode, pretty);
}

#[allow(clippy::too_many_arguments)]
fn traverse(
    db: &Db,
    scope: Scope,
    seed: &str,
    max_hops: u8,
    direction: Direction,
    edge_type: Vec<String>,
    as_of: Option<i64>,
    time_axis: TimeAxis,
    valid_interval: Option<ValidInterval>,
    pretty: bool,
) -> ! {
    let seed = match NodeId::from_str(seed) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid seed id {seed:?}: {e}"), 2),
    };
    // Validate as_of: must be positive if provided.
    if let Some(ts) = as_of {
        if ts <= 0 {
            output::fail(
                "rejected",
                "as-of must be a positive Unix-millisecond timestamp",
                2,
            );
        }
    }
    // Composition rules for an interval predicate, checked here so the
    // errors name CLI flags (the engine re-checks with its own wording).
    if valid_interval.is_some() {
        if as_of.is_some() {
            output::fail(
                "rejected",
                "a --valid-* interval predicate and --as-of are mutually exclusive — a \
                 point-in-time query is a degenerate --valid-overlaps",
                2,
            );
        }
        if time_axis == TimeAxis::Recorded {
            output::fail(
                "rejected",
                "--valid-* interval predicates gate the valid axis only — omit --time-axis recorded",
                2,
            );
        }
    }
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
        as_of,
        time_axis,
    };
    let sg = match valid_interval {
        Some(iv) => db.traverse_interval(&query, iv),
        None => db.traverse(&query),
    };
    let sg = match sg {
        Ok(sg) => sg,
        Err(e) => output::fail_engine(&e),
    };
    let subgraph = match topodb_json::subgraph_to_json(&sg) {
        Ok(v) => v,
        Err(e) => output::fail("internal", &e, 1),
    };
    output::ok(&serde_json::json!({ "subgraph": subgraph }), pretty);
}

/// Run `fetch` with the normalized form of `edge_type`, then — when the raw
/// form differs — again with the raw form, merging results (legacy edges carry
/// the raw spelling). `None` = no type filter, single probe. Engine errors
/// exit via `output::fail_engine`, matching every other CLI fetch.
fn fetch_typed<F>(edge_type: Option<&str>, fetch: F) -> Vec<EdgeRecord>
where
    F: Fn(Option<&str>) -> Result<Vec<EdgeRecord>, TopoError>,
{
    let run = |t: Option<&str>| fetch(t).unwrap_or_else(|e| output::fail_engine(&e));
    match edge_type {
        None => run(None),
        Some(raw) => {
            let norm = match topodb_json::normalize_edge_type(raw) {
                Ok(n) => n,
                Err(e) => output::fail("rejected", &e, 2),
            };
            let mut es = run(Some(&norm));
            if norm != raw {
                es.extend(run(Some(raw)));
            }
            es
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn get_edges(
    db: &Db,
    scope: Scope,
    from: &str,
    to: Option<&str>,
    edge_type: Option<&str>,
    open_only: Option<bool>,
    as_of: Option<i64>,
    direction: cli::DirectionArg,
    time_axis: TimeAxis,
    valid_interval: Option<ValidInterval>,
    pretty: bool,
) -> ! {
    // Validate as_of timestamp first (so as_of: 0 gets the timestamp error,
    // not the exclusivity one), matching the MCP handler error order.
    if let Some(timestamp) = as_of {
        if timestamp <= 0 {
            output::fail(
                "rejected",
                "as-of must be a positive Unix-millisecond timestamp",
                2,
            );
        }
    }

    let from_id = match NodeId::from_str(from) {
        Ok(id) => id,
        Err(e) => output::fail("rejected", &format!("invalid from id {from:?}: {e}"), 2),
    };
    #[allow(clippy::manual_map)]
    let to_id = match to {
        Some(s) => Some(match NodeId::from_str(s) {
            Ok(id) => id,
            Err(e) => output::fail("rejected", &format!("invalid to id {s:?}: {e}"), 2),
        }),
        None => None,
    };

    // Check mutually exclusive parameters: as_of and open_only cannot both
    // be specified. When as_of is present, omit open_only entirely.
    if as_of.is_some() && open_only.is_some() {
        output::fail(
            "rejected",
            "as_of and open_only are mutually exclusive — omit open_only when passing as_of (as_of already means \"open at that instant\")",
            2,
        );
    }

    let scopes = topodb_json::scope_to_scope_set(scope);

    // An interval predicate REPLACES the open-only/as-of gating: an explicit
    // temporal flag alongside it is a conflict, and only the valid axis is
    // in scope. Passing predicates then routes to the `*_interval` reads,
    // which gate on the adjacency interval fields (no post-filter here).
    if valid_interval.is_some() {
        if as_of.is_some() {
            output::fail(
                "rejected",
                "a --valid-* interval predicate and --as-of are mutually exclusive — a \
                 point-in-time query is a degenerate --valid-overlaps",
                2,
            );
        }
        if open_only.is_some() {
            output::fail(
                "rejected",
                "a --valid-* interval predicate replaces --open-only — omit --open-only; the \
                 predicate already says which edges qualify",
                2,
            );
        }
        if time_axis == TimeAxis::Recorded {
            output::fail(
                "rejected",
                "--valid-* interval predicates gate the valid axis only — omit --time-axis recorded",
                2,
            );
        }
    }

    // Determine whether to fetch only open edges: when as_of is present,
    // always fetch with open_only=false to see the full history, then filter
    // below. When as_of is absent, use the provided open_only or default to true.
    let open_only_to_use = if as_of.is_some() {
        false
    } else {
        open_only.unwrap_or(true)
    };

    // Fold the interval conditional INSIDE the fetch closures so there is one
    // shared tail (mirrors the MCP server's shape at server.rs ~4153-4172).
    let fetch_from = |t: Option<&str>| match valid_interval {
        Some(iv) => db.edges_from_interval(&scopes, from_id, to_id, t, iv),
        None => db.edges_from(&scopes, from_id, to_id, t, open_only_to_use, time_axis),
    };
    let fetch_to = |t: Option<&str>| match valid_interval {
        Some(iv) => db.edges_to_interval(&scopes, from_id, to_id, t, iv),
        None => db.edges_to(&scopes, from_id, to_id, t, open_only_to_use, time_axis),
    };

    let mut edges = match direction {
        cli::DirectionArg::Out => fetch_typed(edge_type, fetch_from),
        cli::DirectionArg::In => fetch_typed(edge_type, fetch_to),
        cli::DirectionArg::Both => {
            let mut es = fetch_typed(edge_type, fetch_from);
            es.extend(fetch_typed(edge_type, fetch_to));
            es
        }
    };

    edges.sort_by_key(|e| e.id);
    edges.dedup_by_key(|e| e.id);

    // If as_of is set, filter edges to only those live at that timestamp on
    // the requested axis (inclusive lower bound, exclusive upper bound):
    // valid axis gates on valid_from/valid_to (world time), recorded axis
    // gates on recorded_at/superseded_at (belief time).
    if let Some(timestamp) = as_of {
        match time_axis {
            TimeAxis::Valid => edges.retain(|e| topodb_json::edge_live_at(e, timestamp)),
            TimeAxis::Recorded => edges.retain(|e| topodb_json::edge_believed_at(e, timestamp)),
        }
    }

    let edges: Vec<serde_json::Value> = match edges
        .iter()
        .map(topodb_json::edge_to_json)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(edges) => edges,
        Err(e) => output::fail("internal", &e, 1),
    };

    output::ok(&serde_json::json!({ "edges": edges }), pretty);
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

/// Phase C decay sweep: delegate to the shared
/// `topodb_json::lifecycle_candidates` (read-only, unbumped) and print
/// the evidence array. Day flags convert to ms here — the shared layer
/// speaks ms.
#[allow(clippy::too_many_arguments)]
fn lifecycle_candidates(
    db: &Db,
    scope: Scope,
    limit: usize,
    half_life_episodic_days: f64,
    half_life_semantic_days: f64,
    half_life_procedural_days: f64,
    half_life_decision_days: f64,
    now_ms: Option<i64>,
    pretty: bool,
) -> ! {
    let scopes = topodb_json::scope_to_scope_set(scope);
    let params = topodb_json::LifecycleParams {
        limit,
        half_life_episodic_ms: (half_life_episodic_days * 86_400_000.0) as i64,
        half_life_semantic_ms: (half_life_semantic_days * 86_400_000.0) as i64,
        half_life_procedural_ms: (half_life_procedural_days * 86_400_000.0) as i64,
        half_life_decision_ms: (half_life_decision_days * 86_400_000.0) as i64,
    };
    let now = now_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    });
    let candidates = match topodb_json::lifecycle_candidates(db, &scopes, &params, now) {
        Ok(c) => c,
        Err(topodb_json::ComposeError::Invalid(m)) => output::fail("rejected", &m, 2),
        Err(topodb_json::ComposeError::Engine(e)) => output::fail_engine(&e),
    };
    let value = match serde_json::to_value(&candidates) {
        Ok(v) => v,
        Err(e) => output::fail("internal", &e.to_string(), 1),
    };
    output::ok(&value, pretty);
}

/// Phase E purge: plan via the shared topodb_json builder, then either
/// report (dry-run, the default — no write path is even reachable) or
/// submit the whole RemoveNode batch atomically. An empty batch under
/// --yes skips the submit and reports seq: null.
fn purge(db: &Db, scope: Scope, tombstoned_before: i64, yes: bool, pretty: bool) -> ! {
    let scopes = topodb_json::scope_to_scope_set(scope);
    let (ops, ids) = match topodb_json::plan_purge(db, &scopes, tombstoned_before) {
        Ok(plan) => plan,
        Err(topodb_json::ComposeError::Invalid(m)) => output::fail("rejected", &m, 2),
        Err(topodb_json::ComposeError::Engine(e)) => output::fail_engine(&e),
    };
    let count = ids.len();
    if !yes {
        output::ok(
            &serde_json::json!({ "dry_run": true, "count": count, "ids": ids }),
            pretty,
        );
    }
    let seq = if ops.is_empty() {
        serde_json::Value::Null
    } else {
        match db.submit(ops) {
            Ok(applied) => serde_json::json!(applied.last_seq),
            Err(e) => output::fail_engine(&e),
        }
    };
    output::ok(
        &serde_json::json!({ "dry_run": false, "count": count, "ids": ids, "seq": seq }),
        pretty,
    );
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
    // Parse --props first so malformed JSON is rejected (exit 2) even on a dedup hit.
    let extra = parse_props_arg(props);
    // Validate reserved keys BEFORE the dedup check (so reserved keys are always rejected).
    let props = match topodb_json::memory_props(&content, extra.as_ref()) {
        Ok(p) => p,
        Err(e) => output::fail("rejected", &e, 2),
    };
    // Dedup: re-storing an identical (whitespace-normalized) fact returns
    // the existing node — same contract as the MCP create_memory tool.
    match topodb_json::existing_memory(db, scope, &content) {
        Ok(Some(id)) => output::ok(
            &serde_json::json!({ "id": id.to_string(), "deduplicated": true }),
            pretty,
        ),
        Ok(None) => {}
        Err(e) => output::fail_engine(&e),
    }
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
    output::ok(
        &serde_json::json!({ "id": id.to_string(), "deduplicated": false }),
        pretty,
    );
}

fn create_entity(
    db: &Db,
    scope: Scope,
    name: String,
    props: Option<&str>,
    always_create: bool,
    pretty: bool,
) -> ! {
    let extra = parse_props_arg(props);
    if !always_create {
        // Same collision surface as `remember`: write scope + shared.
        let lookup = topodb_json::scopes_to_scope_set(&[scope, Scope::Shared]);
        let existing = match topodb_json::find_existing_entity(db, &lookup, &name) {
            Ok(hit) => hit,
            Err(e) => output::fail_engine(&e),
        };
        if let Some(node) = existing {
            // Merge only NEW metadata keys; never overwrite, never touch name.
            let incoming = match topodb_json::merge_required_prop(
                topodb_json::ENTITY_NAME_PROP,
                PropValue::Str(name.clone()),
                extra.as_ref(),
            ) {
                Ok(p) => p,
                Err(e) => output::fail("rejected", &e, 2),
            };
            let new_keys: std::collections::BTreeMap<String, Option<PropValue>> = incoming
                .into_iter()
                .filter(|(k, _)| k != topodb_json::ENTITY_NAME_PROP && !node.props.contains_key(k))
                .map(|(k, v)| (k, Some(v)))
                .collect();
            if !new_keys.is_empty() {
                if let Err(e) = db.submit(vec![Op::SetNodeProps {
                    id: node.id,
                    props: new_keys,
                }]) {
                    output::fail_engine(&e);
                }
            }
            output::ok(
                &serde_json::json!({ "id": node.id.to_string(), "created": false }),
                pretty,
            );
        }
    }
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
    output::ok(
        &serde_json::json!({ "id": id.to_string(), "created": true }),
        pretty,
    );
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
        ty: ty.clone().into(),
        from,
        to,
        props,
        valid_from,
        recorded_at: None,
    };
    if let Err(e) = db.submit(vec![op]) {
        output::fail_engine(&e);
    }
    let write_set = topodb_json::scope_to_scope_set(scope);
    let conflicts: Vec<serde_json::Value> = db
        .edges_from(&write_set, from, None, Some(&ty), true, TimeAxis::Valid)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.id != id)
        .map(|e| {
            serde_json::json!({
                "edge_id": e.id.to_string(),
                "to": e.to.to_string(),
                "valid_from": e.valid_from,
            })
        })
        .collect();
    let mut value = serde_json::json!({ "id": id.to_string() });
    if !conflicts.is_empty() {
        value["conflicts"] = serde_json::Value::Array(conflicts);
    }
    output::ok(&value, pretty);
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
    kind: Option<String>,
    text_mode: bool,
    pretty: bool,
) -> ! {
    let extra = parse_props_arg(props);
    let req = topodb_json::RememberRequest {
        content,
        entities,
        edge_type,
        supersedes,
        props: extra,
        kind,
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
        new_memory,
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
    let supersession_candidates: Vec<serde_json::Value> = match &new_memory {
        Some(content) => {
            let scope_set = topodb_json::scope_to_scope_set(scope);
            let content_tokens = topodb_json::tokens(content);
            let mut scored: Vec<(String, String, f64)> = db
                .search_text_unbumped(&scope_set, content, topodb_json::TEXT_NEAR_DUP_CANDIDATES)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(n, _)| {
                    if n.label != topodb_json::MEMORY_LABEL
                        || topodb_json::MEMORY_TOMBSTONE_PROPS
                            .iter()
                            .any(|p| n.props.contains_key(*p))
                        || n.id == memory_id
                    {
                        return None;
                    }
                    let existing = match n.props.get(topodb_json::MEMORY_CONTENT_PROP) {
                        Some(PropValue::Str(c)) => c.clone(),
                        _ => return None,
                    };
                    let existing_tokens = topodb_json::tokens(&existing);
                    let containment =
                        topodb_json::containment_of_sets(&content_tokens, &existing_tokens);
                    if containment >= topodb_json::TEXT_NEAR_DUP_CONTAINMENT {
                        Some((n.id.to_string(), existing, containment))
                    } else {
                        None
                    }
                })
                .collect();
            // Rank by containment (descending) and cap at the same top-K as MCP's
            // ranked near-duplicate probe, for parity between the two write paths.
            scored.sort_by(|a, b| b.2.total_cmp(&a.2));
            scored.truncate(topodb_json::NEAR_DUP_K);
            scored
                .into_iter()
                .map(|(id, existing, containment)| {
                    serde_json::json!({
                        "memory_id": id,
                        "relation": topodb_json::dup_relation(content, &existing),
                        "score": containment,
                    })
                })
                .collect()
        }
        None => Vec::new(),
    };
    let entities: Vec<serde_json::Value> = entities
        .into_iter()
        .map(
            |e| serde_json::json!({ "name": e.name, "id": e.id.to_string(), "created": e.created }),
        )
        .collect();
    let mut value = serde_json::json!({
        "memory_id": memory_id.to_string(),
        "deduplicated": deduplicated,
        "entities": entities,
        "edge_ids": edge_ids,
        "superseded": superseded,
    });
    if !supersession_candidates.is_empty() {
        value["supersession_candidates"] = serde_json::Value::Array(supersession_candidates);
    }
    let text = {
        let id = value["memory_id"].as_str().unwrap_or("?");
        let mut line = format!("remembered {id}");
        if value["deduplicated"].as_bool().unwrap_or(false) {
            line.push_str(" (deduplicated)");
        }
        let sup = value["superseded"].as_array().map(|a| a.len()).unwrap_or(0);
        if sup > 0 {
            line.push_str(&format!(" superseding {sup}"));
        }
        Some(line)
    };
    output::render(&value, text, text_mode, pretty);
}

/// Soft-retire memories: plan via `topodb_json::plan_forget` (strict — any
/// invalid id rejects the whole call), submit the one batch, echo the ids.
fn forget(db: &Db, scope: Scope, ids: &[String], text_mode: bool, pretty: bool) -> ! {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (ops, forgotten) = match topodb_json::plan_forget(db, scope, ids, now) {
        Ok(plan) => plan,
        Err(topodb_json::ComposeError::Invalid(m)) => output::fail("rejected", &m, 2),
        Err(topodb_json::ComposeError::Engine(e)) => output::fail_engine(&e),
    };
    if let Err(e) = db.submit(ops) {
        output::fail_engine(&e);
    }
    let value = serde_json::json!({ "forgotten": forgotten });
    let ids: Vec<&str> = value["forgotten"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let text = Some(format!("forgot {}: {}", ids.len(), ids.join(" ")));
    output::render(&value, text, text_mode, pretty);
}

fn obsidian_ingest(
    db: &Db,
    vault: &std::path::Path,
    write_scope: Scope,
    dry_run: bool,
    pretty: bool,
) -> ! {
    let lookup = topodb_json::scopes_to_scope_set(&[write_scope, Scope::Shared]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    match topodb_obsidian::ingest_vault(db, vault, write_scope, &lookup, now, dry_run, None) {
        Ok(report) => output::ok(
            &serde_json::to_value(&report).expect("report serializes"),
            pretty,
        ),
        Err(m) => output::fail("rejected", &m, 2),
    }
}

#[allow(clippy::too_many_arguments)]
fn obsidian_seed(
    db: &Db,
    vault: &std::path::Path,
    query: Option<String>,
    k: usize,
    entity: Option<String>,
    hops: u8,
    overwrite: bool,
    scope: Scope,
    pretty: bool,
) -> ! {
    let scopes = topodb_json::scopes_to_scope_set(&[scope, Scope::Shared]);
    let memories = match (&query, &entity) {
        (Some(q), None) => topodb_obsidian::select_by_query(db, &scopes, q, k, None)
            .unwrap_or_else(|e| output::fail_engine(&e)),
        (None, Some(name)) => match topodb_obsidian::select_by_entity(db, &scopes, name, hops) {
            Ok(m) => m,
            Err(topodb_json::ComposeError::Invalid(m)) => output::fail("rejected", &m, 2),
            Err(topodb_json::ComposeError::Engine(e)) => output::fail_engine(&e),
        },
        _ => output::fail(
            "rejected",
            "exactly one of --query or --entity is required",
            2,
        ),
    };
    match topodb_obsidian::seed_vault(db, &scopes, vault, &memories, overwrite) {
        Ok(report) => output::ok(
            &serde_json::to_value(&report).expect("report serializes"),
            pretty,
        ),
        Err(m) => output::fail("rejected", &m, 2),
    }
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
    let applied = match db.submit(vec![Op::CloseEdge {
        id,
        valid_to,
        superseded_at: None,
    }]) {
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
