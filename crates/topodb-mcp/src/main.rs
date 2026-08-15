//! `topodb-mcp` — a stdio MCP server / resident daemon exposing the TopoDB agent-memory engine.
//!
//! Usage: `topodb-mcp --db <path> [--scope <ulid|shared>]
//!         [--read-scopes <ulid|shared>[,...]] [--spec <spec.json>]
//!         [--allow-unscoped-changes] [--embeddings <off|model>]
//!         [--model-dir <path>] [--no-ort-download] [--socket [PATH]]` — see
//! `config`'s module doc for what each flag controls.
//!
//! Without `--socket` (the default), the process speaks newline-delimited JSON-RPC
//! over stdio (rmcp's `stdio` transport). stdout is reserved for the protocol; all
//! diagnostics go to stderr.
//!
//! With `--socket [PATH]`, the process runs as a resident daemon: opens the DB,
//! claims the exclusive lock via election, binds the socket endpoint, and serves
//! multiple clients concurrently over that endpoint. Stdout is not used for
//! protocol in this mode.

mod config;
mod daemon;
mod embedder;
mod onboard_boot;
mod ort_fetch;
mod server;
mod socket_path;

use std::error::Error;
use std::path::PathBuf;

use rmcp::transport::stdio;
use rmcp::ServiceExt;
use topodb::Db;

use crate::config::Config;
use crate::server::TopoServer;

/// Default embedding-model cache directory when `--model-dir` is omitted:
/// `$HOME/.cache/topodb/models`, falling back to `.topodb-models` in the
/// current directory if `$HOME` is unset (e.g. some sandboxed/CI shells).
fn default_model_cache_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home)
            .join(".cache")
            .join("topodb")
            .join("models"),
        _ => PathBuf::from(".topodb-models"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_args(std::env::args().skip(1))?;

    // A missing db FILE is fine (Db creates it), but a missing parent DIRECTORY
    // is a configuration error we can catch up front with a clear message
    // rather than a lower-level storage error.
    if let Some(parent) = config.db_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "database parent directory does not exist: {}",
                parent.display()
            )
            .into());
        }
    }

    // Daemon mode: --socket runs the resident server instead of stdio mode.
    if config.socket.is_some() {
        match daemon::serve(&config).await {
            Ok(daemon::ServeOutcome::Served) => std::process::exit(0),
            Ok(daemon::ServeOutcome::LostElection) => {
                eprintln!("another daemon holds this db; exiting (lost election)");
                std::process::exit(0);
            }
            Err(e) => return Err(format!("{e}").into()),
        }
    }

    // Open using the db's own persisted index spec unless the caller passed an
    // explicit `--spec`. This mirrors `topodb-cli`: an EXISTING file inherits
    // its persisted spec exactly (via `open_stored`), so a db another front end
    // (topodb-cli, or a prior `--spec` run) already populated is never
    // reindexed nor its declared equality indexes silently dropped. A brand-new
    // file is created with the canonical `default_spec()` — byte-identical to
    // what topodb-cli writes — so either tool can later serve it with no
    // reindex. An explicit `--spec` is honored verbatim (and may reindex): the
    // power-user seam for creating a db with a custom spec or forcing a
    // re-declare. `Path::exists` is safe here — the parent-dir check above ran,
    // and a stdio MCP server is a single writer per db path.
    // Shared parse/default policy (`topodb_json::lock_wait_budget_ms`); a
    // long-running server config'd by hand warns once and uses the default
    // rather than dying on a typo.
    let (budget_ms, lock_wait_warn) =
        topodb_json::lock_wait_budget_ms(std::env::var("TOPODB_LOCK_WAIT_MS").ok().as_deref());
    if let Some(warn) = lock_wait_warn {
        eprintln!("topodb-mcp: {warn}");
    }
    let db = topodb_json::open_with_busy_retry(budget_ms, || {
        match &config.spec {
            Some(spec) => Db::open_with(&config.db_path, spec.clone()),
            None if config.db_path.exists() => {
                // Inherit the persisted spec — but a db still on an older STOCK
                // default (never `--spec`-customized) is silently upgraded to the
                // current default (`topodb_json::upgraded_spec`), e.g. picking up
                // the (Entity, name) text index so entities are searchable by
                // name. A one-time reindex on open is the cost. Customized specs
                // are inherited verbatim, exactly as before.
                let db = Db::open_stored(&config.db_path)?;
                let persisted = db.index_spec();
                let upgraded = topodb_json::upgraded_spec(persisted.clone());
                if upgraded != persisted {
                    drop(db);
                    Db::open_with(&config.db_path, upgraded)
                } else {
                    Ok(db)
                }
            }
            None => Db::open_with(&config.db_path, config::default_spec()),
        }
    })?;
    // Best-effort install-time onboarding: ensure CONVENTIONS.md sits next
    // to the db and catch up on overdue hygiene. `topodb-mcp` is spawned by
    // every code client, so server boot is the one client-agnostic trigger
    // — and since the resident daemon idle-exits in ~60s, this is the
    // PRIMARY hygiene path, not a backup. Never allowed to fail or slow
    // boot (see `onboard_boot`'s module doc).
    let onboard_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    onboard_boot::run_boot_onboarding(&db, &config.db_path, config.default_scope, onboard_now_ms);
    // `--embeddings off` (case-insensitive) => permanently disabled.
    // Omitted, OR `--embeddings auto` (case-insensitive) => auto (start with
    // the default model) — "auto" is accepted as an explicit spelling of the
    // same default the flag already has when omitted, so a caller that always
    // passes `--embeddings` doesn't need a special case for "use the
    // default." Any other value is a model name to start with; an
    // unrecognized one still starts an `Embedder`, it just lands in `Failed`
    // status (see `embedder::Embedder`).
    let embedder = match config.embeddings.as_deref() {
        Some(s) if s.eq_ignore_ascii_case("off") => embedder::Embedder::disabled(),
        Some(s) if s.eq_ignore_ascii_case("auto") => embedder::Embedder::start(
            None,
            config
                .model_dir
                .clone()
                .unwrap_or_else(default_model_cache_dir),
            !config.no_ort_download,
        ),
        other => embedder::Embedder::start(
            other.map(str::to_string),
            config
                .model_dir
                .clone()
                .unwrap_or_else(default_model_cache_dir),
            !config.no_ort_download,
        ),
    };

    // Backfill: embed nodes that predate the model (or missed their write-
    // time embedding). Low-priority: small batches, sleeps between, skip
    // silently on any error — never competes with tool calls for long.
    {
        let db = db.clone();
        let embedder = embedder.clone();
        std::thread::spawn(move || {
            use topodb::{Op, PropValue};
            use topodb_json::{
                ALIAS_LABEL, ALIAS_NAME_PROP, ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP,
                MEMORY_LABEL,
            };
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if embedder.status() != crate::embedder::EmbedderStatus::Ready {
                    if embedder.status() == crate::embedder::EmbedderStatus::Failed
                        || embedder.status() == crate::embedder::EmbedderStatus::Off
                    {
                        return; // terminal states: nothing to backfill, ever
                    }
                    continue; // still downloading
                }
                // NOTE scoping: backfill must see every scope. nodes_by_label
                // takes a ScopeSet; enumerate scopes via db.debug… — NOT
                // available. Instead: backfill covers the change feed from
                // seq 1 (ops_since is unscoped by design — the same
                // host-level carve-out get_changes uses).
                // The op log is the ONE unscoped read (`ops_since` — the
                // same host-level carve-out get_changes uses), which makes
                // it the only way a backfill can see every scope without a
                // per-scope enumeration API. Single pass: collect embedded
                // (under this model) and removed ids, then embed the
                // misses. A compacted op log makes `ops_since(1)` return
                // `Err(TopoError::Compacted { .. })` instead of full
                // history — the `let Ok(events) = ... else { return }`
                // below then exits this thread for good, so a db that has
                // ever been compacted permanently skips backfill; an
                // accepted limitation, not a bug, since there is no other
                // unscoped read to fall back to. Memory/Entity/Alias text
                // here is read from CreateNode-time props only; content
                // edited afterward via SetNodeProps is only re-embedded on
                // the next process start — acceptable, noted.
                let model = embedder.model_name();
                let mut batched = 0usize;
                let Ok(events) = db.ops_since(1) else { return };
                let mut embedded: std::collections::HashSet<topodb::NodeId> = Default::default();
                let mut removed: std::collections::HashSet<topodb::NodeId> = Default::default();
                let mut candidates: Vec<(topodb::NodeId, String)> = Vec::new();
                for ev in &events {
                    match ev.op.as_ref() {
                        Op::SetEmbedding { id, model: m, .. } if *m == model => {
                            embedded.insert(*id);
                        }
                        Op::RemoveNode { id } => {
                            removed.insert(*id);
                        }
                        Op::CreateNode {
                            id, label, props, ..
                        } => {
                            let text = match label.as_str() {
                                l if l == MEMORY_LABEL => props.get(MEMORY_CONTENT_PROP),
                                l if l == ENTITY_LABEL => props.get(ENTITY_NAME_PROP),
                                l if l == ALIAS_LABEL => props.get(ALIAS_NAME_PROP),
                                _ => None,
                            };
                            if let Some(PropValue::Str(t)) = text {
                                candidates.push((*id, t.clone()));
                            }
                        }
                        _ => {}
                    }
                }
                for (id, text) in candidates {
                    if embedded.contains(&id) || removed.contains(&id) {
                        continue;
                    }
                    let Some(vector) = embedder.embed(&text) else {
                        continue;
                    };
                    let _ = db.submit(vec![Op::SetEmbedding {
                        id,
                        model: model.clone(),
                        vector,
                    }]);
                    batched += 1;
                    if batched.is_multiple_of(16) {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                }
                return; // one full pass per process start is enough
            }
        });
    }

    let server = TopoServer::new(db, &config, embedder);

    // `serve` completes the initialize handshake; `waiting` blocks until the
    // client disconnects (stdin EOF) or the service errors.
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
