//! JSON emit + exit-code helpers shared by every `topodb-cli` command.
//!
//! The contract (see the plan's Global Constraints): success is a JSON value
//! on stdout with exit 0; failure is `{"error":{"kind","message"}}` on stderr
//! with exit 2 for a rejected/bad-input condition (engine `Rejected`, scope
//! parse failure, ...) or exit 1 for an internal/storage/db-open failure.
//! clap's own usage errors (missing `--db`, unknown subcommand, ...) are left
//! to clap, whose default exit code is already 2 — callers never route those
//! through `fail`.

use serde_json::Value;

/// Print `value` to stdout — compact JSON, or pretty-printed if `pretty` is
/// set — and exit 0. Never returns.
pub fn ok(value: &Value, pretty: bool) -> ! {
    let rendered = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match rendered {
        Ok(rendered) => {
            println!("{rendered}");
            std::process::exit(0);
        }
        // Unreachable in practice (a serde_json::Value always serializes),
        // but the no-panic-on-runtime-paths contract means even this goes
        // through fail() rather than expect()/unwrap().
        Err(e) => fail("internal", &format!("serializing output: {e}"), 1),
    }
}

/// Print `{"error":{"kind": kind, "message": message}}` to stderr and exit
/// `code`. Never returns. `code` is the caller's choice: 2 for a
/// rejected/bad-input condition, 1 for an internal/storage failure.
pub fn fail(kind: &str, message: &str, code: i32) -> ! {
    let body = serde_json::json!({ "error": { "kind": kind, "message": message } });
    eprintln!("{body}");
    std::process::exit(code);
}

/// Maps a `TopoError` to the right `(kind, exit-code)` pair and calls
/// [`fail`]: `Rejected` (bad input the caller can fix — an undeclared index,
/// an empty batch, a malformed query) is `("rejected", 2)`; every other
/// variant (`Storage`, `Encoding`, `Compacted`, `Closed`,
/// `UnsupportedFormat`, and any future `#[non_exhaustive]` addition) is
/// `("internal", 1)` — the caller can't fix those by changing their input.
pub fn fail_engine(e: &topodb::TopoError) -> ! {
    match e {
        topodb::TopoError::Rejected(_) => fail("rejected", &e.to_string(), 2),
        _ => fail("internal", &e.to_string(), 1),
    }
}
