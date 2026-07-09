mod cli;
mod output;

use clap::Parser;
use cli::{Cli, Command};
use topodb::{Db, Scope};

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
