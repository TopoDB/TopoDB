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

    let db = Db::open_with(&config.db_path, config.spec.clone())?;
    let server = TopoServer::new(db, &config);

    // `serve` completes the initialize handshake; `waiting` blocks until the
    // client disconnects (stdin EOF) or the service errors.
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
