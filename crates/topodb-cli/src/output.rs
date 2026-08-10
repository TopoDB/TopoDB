//! JSON emit + exit-code helpers shared by every `topodb-cli` command.
//!
//! The contract (see the plan's Global Constraints): success is a JSON value
//! on stdout with exit 0; failure is `{"error":{"kind","message"}}` on stderr
//! with exit 3 for a lock contention error (`Busy`), exit 2 for a
//! rejected/bad-input condition (engine `Rejected`, scope parse failure, ...),
//! or exit 1 for an internal/storage/db-open failure. clap's own usage errors
//! (missing `--db`, unknown subcommand, ...) are left to clap, whose default
//! exit code is already 2 — callers never route those through `fail`.

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
/// an empty batch, a malformed query) is `("rejected", 2)`; `Busy` (lock
/// contention) is `("busy", 3)`; every other variant (`Storage`, `Encoding`,
/// `Compacted`, `Closed`, `UnsupportedFormat`) is `("internal", 1)` — the
/// caller can't fix those by changing their input.
pub fn fail_engine(e: &topodb::TopoError) -> ! {
    match e {
        topodb::TopoError::Rejected(_) => fail("rejected", &e.to_string(), 2),
        topodb::TopoError::Busy => fail("busy", &e.to_string(), 3),
        _ => fail("internal", &e.to_string(), 1),
    }
}

/// Emit either the text form (when `text_mode` and a text rendering exists)
/// or the JSON value. `text` is `None` for commands without a text renderer,
/// which fall back to JSON even in text mode. Never returns.
pub fn render(json: &Value, text: Option<String>, text_mode: bool, pretty: bool) -> ! {
    if text_mode {
        if let Some(t) = text {
            println!("{t}");
            std::process::exit(0);
        }
    }
    ok(json, pretty)
}

/// One-line stderr note disambiguating "empty result" from "wrong scope"
/// (fixes D1). Printed only for empty reads; stdout is untouched.
pub fn empty_scope_echo(scope: &str, source: &str) {
    eprintln!("topodb: 0 matches in scope {scope} (source: {source})");
}

/// One-line stderr note naming an applied created-time filter — the CLI
/// twin of the MCP result's `applied_time_filter`. Printed whenever a
/// filter ran, explicit or rewritten; stdout is untouched. The resolved
/// UTC interval is rendered too, so relative phrases ("last week") show
/// the window that actually ran.
pub fn time_filter_echo(desc: &str, source: &str, after_ms: Option<i64>, before_ms: Option<i64>) {
    let bound = |ms: Option<i64>| ms.map_or("..".to_string(), iso_utc);
    eprintln!(
        "topodb: time filter: {desc} = [{}, {}) (source: {source})",
        bound(after_ms),
        bound(before_ms),
    );
}

/// Render epoch ms as a compact UTC ISO instant (date only when the time
/// is exactly midnight). Inverse of the Hinnant `days_from_civil` used by
/// the parser — no chrono dependency for one stderr line.
fn iso_utc(ms: i64) -> String {
    const DAY_MS: i64 = 86_400_000;
    let days = ms.div_euclid(DAY_MS);
    let rem = ms.rem_euclid(DAY_MS);
    // Hinnant civil_from_days (valid for the parser's 1970–2099 range).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    if rem == 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        let (h, min, s) = (rem / 3_600_000, (rem / 60_000) % 60, (rem / 1000) % 60);
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::iso_utc;

    /// Anchors reuse the parser test suite's hand-derived constants.
    #[test]
    fn iso_utc_matches_the_parser_anchors() {
        assert_eq!(iso_utc(1_785_542_400_000), "2026-08-01");
        assert_eq!(iso_utc(1_767_225_600_000), "2026-01-01");
        assert_eq!(
            iso_utc(1_785_542_400_000 + 55_800_000),
            "2026-08-01T15:30:00Z"
        );
        // Leap day 2028-02-29 (day 21243 since the epoch).
        assert_eq!(iso_utc(21_243 * 86_400_000), "2028-02-29");
    }
}
