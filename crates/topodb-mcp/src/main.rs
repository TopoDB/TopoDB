//! `topodb-mcp` — a stdio MCP server exposing the TopoDB agent-memory engine.
//!
//! Usage: `topodb-mcp --db <path> [--scope <ulid|shared>] [--spec <spec.json>]`
//!
//! The process speaks newline-delimited JSON-RPC over stdio (rmcp's `stdio`
//! transport). stdout is reserved for the protocol; all diagnostics go to
//! stderr.

mod config;
mod server;

use std::error::Error;

use rmcp::transport::stdio;
use rmcp::ServiceExt;
use topodb::Db;

use crate::config::Config;
use crate::server::TopoServer;

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
    let db = match &config.spec {
        Some(spec) => Db::open_with(&config.db_path, spec.clone())?,
        None if config.db_path.exists() => Db::open_stored(&config.db_path)?,
        None => Db::open_with(&config.db_path, config::default_spec())?,
    };
    let server = TopoServer::new(db, &config);

    // `serve` completes the initialize handshake; `waiting` blocks until the
    // client disconnects (stdin EOF) or the service errors.
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
