//! `clap`-derive CLI surface: global args + the subcommand enum. Tasks 4-5
//! add write/read subcommands to [`Command`]; only `Info` exists so far.

use std::path::PathBuf;

#[derive(clap::Parser)]
#[command(
    name = "topodb",
    about = "Direct-embedded CLI over a TopoDB database file"
)]
pub struct Cli {
    /// Database file (or TOPODB_DB env).
    #[arg(long, env = "TOPODB_DB")]
    pub db: PathBuf,
    /// Default scope: a ScopeId ULID, or "shared".
    #[arg(long, default_value = "shared")]
    pub scope: String,
    /// Pretty-print JSON output.
    #[arg(long)]
    pub pretty: bool,
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Report the open database's path, format version, current op-log
    /// sequence number, index spec, and default scope.
    Info,
    // Tasks 4-5 add the rest (create-memory, create-entity, link, get,
    // find, search, traverse, access-stats, changes).
}
