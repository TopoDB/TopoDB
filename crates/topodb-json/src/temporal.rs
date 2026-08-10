//! Deterministic temporal query rewriting for temporal recall: extract ONE
//! temporal phrase from a search query and resolve it to a created-time
//! range. Pure — `now_ms` is always injected by the caller; this module
//! never reads a clock, so every result (and every test) is reproducible.
//!
//! Recognized phrases (case-insensitive; the first rule in priority order
//! that matches anywhere in the query wins):
//!
//! 1. `between <date> and <date>`      → `[start(a), end(b))` — both
//!    endpoint periods included
//! 2. `before <date>` / `until <date>` → `[.., start(date))`
//! 3. `after <date>` / `since <date>`  → `[start(date), ..)` — inclusive
//! 4. `last <N> days`                  → `[today_start − N days, ..)`
//! 5. `yesterday` | `today` | `last week` | `last month`
//! 6. bare `<date>`, optionally after `in`/`on`/`during` → `[start, end)`
//!
//! `<date>` is ISO — `2026-08-01`, `2026-08`, or `2026` — with years
//! restricted to 1970–2099 so ports and issue numbers never parse as
//! years. Date-only bounds resolve to UTC day/month/year boundaries per
//! the spec: `before 2026-08-01` excludes that entire day, `after
//! 2026-08-01` includes it. Rolling windows anchor at the start of the UTC
//! day containing `now_ms`: `last week` = last 7 days, `last month` = last
//! 30 days.
//!
//! Conservative by design, mirroring the spec: no recognized phrase
//! ("before the v8 migration"), a calendar-invalid date, an inverted
//! `between`, or a residual query left empty by the strip ("last week"
//! alone) all return `None` — the caller searches its original query
//! unmodified.

use regex::Regex;
use std::sync::OnceLock;

const DAY_MS: i64 = 86_400_000;

/// A temporal phrase resolved against the injected reference `now_ms`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TemporalRewrite {
    /// The query with the matched phrase removed and whitespace collapsed.
    pub residual_query: String,
    /// Keep nodes created at or after this UTC ms timestamp (`None` = unbounded).
    pub after_ms: Option<i64>,
    /// Keep nodes created strictly before this UTC ms timestamp (`None` = unbounded).
    pub before_ms: Option<i64>,
    /// Exactly what matched, original casing preserved (surfaces later in
    /// `applied_time_filter.matched_phrase`).
    pub matched_phrase: String,
}

/// Priority order is load-bearing: prefixed forms must outrank the bare
/// date, or `since 2026-08-01` would strip only the date and leave "since"
/// in the residual.
#[derive(Clone, Copy)]
enum Rule {
    Between,
    Before,
    After,
    LastNDays,
    Relative,
    BareDate,
}

fn rules() -> &'static [(Rule, Regex)] {
    static RULES: OnceLock<Vec<(Rule, Regex)>> = OnceLock::new();
    RULES.get_or_init(|| {
        // Three capture groups per <date>: (year)(-month)?(-day)?, with the
        // year bounded to 1970–2099.
        let date = r"(19[7-9]\d|20\d{2})(?:-(\d{2})(?:-(\d{2}))?)?";
        // Compile-time-constant patterns, each exercised by the test table —
        // the `expect` is unreachable (crate rule: nothing here panics on
        // caller input; this is not caller input).
        let re = |p: &str| Regex::new(p).expect("static temporal pattern");
        vec![
            (
                Rule::Between,
                re(&format!(r"(?i)\bbetween\s+{date}\s+and\s+{date}\b")),
            ),
            (
                Rule::Before,
                re(&format!(r"(?i)\b(?:before|until)\s+{date}\b")),
            ),
            (
                Rule::After,
                re(&format!(r"(?i)\b(?:after|since)\s+{date}\b")),
            ),
            (Rule::LastNDays, re(r"(?i)\blast\s+(\d{1,4})\s+days?\b")),
            (
                Rule::Relative,
                re(r"(?i)\b(yesterday|today|last\s+week|last\s+month)\b"),
            ),
            (
                Rule::BareDate,
                re(&format!(r"(?i)\b((?:in|on|during)\s+)?{date}\b")),
            ),
        ]
    })
}

/// One `<date>` at day, month, or year granularity, already range-checked.
#[derive(Clone, Copy)]
enum DateSpec {
    Day { y: i64, m: i64, d: i64 },
    Month { y: i64, m: i64 },
    Year { y: i64 },
}

impl DateSpec {
    /// Read the three-group `<date>` starting at capture index `i`,
    /// rejecting calendar-invalid combinations (month 13, Feb 30).
    fn read(caps: &regex::Captures<'_>, i: usize) -> Option<Self> {
        let y: i64 = caps.get(i)?.as_str().parse().ok()?;
        let m: i64 = match caps.get(i + 1) {
            Some(m) => m.as_str().parse().ok()?,
            None => return Some(DateSpec::Year { y }),
        };
        if !(1..=12).contains(&m) {
            return None;
        }
        let d: i64 = match caps.get(i + 2) {
            Some(d) => d.as_str().parse().ok()?,
            None => return Some(DateSpec::Month { y, m }),
        };
        (1..=days_in_month(y, m))
            .contains(&d)
            .then_some(DateSpec::Day { y, m, d })
    }

    /// UTC ms of the period's first instant (`2026-08` → Aug 1 00:00:00Z).
    fn start_ms(self) -> i64 {
        match self {
            DateSpec::Day { y, m, d } => days_from_civil(y, m, d) * DAY_MS,
            DateSpec::Month { y, m } => days_from_civil(y, m, 1) * DAY_MS,
            DateSpec::Year { y } => days_from_civil(y, 1, 1) * DAY_MS,
        }
    }

    /// UTC ms just past the period's last instant (exclusive end).
    fn end_ms(self) -> i64 {
        match self {
            DateSpec::Day { y, m, d } => (days_from_civil(y, m, d) + 1) * DAY_MS,
            DateSpec::Month { y, m: 12 } => days_from_civil(y + 1, 1, 1) * DAY_MS,
            DateSpec::Month { y, m } => days_from_civil(y, m + 1, 1) * DAY_MS,
            DateSpec::Year { y } => days_from_civil(y + 1, 1, 1) * DAY_MS,
        }
    }
}

/// Days since 1970-01-01 for a Gregorian civil date (Howard Hinnant's
/// `days_from_civil`; the year is regex-bounded to 1970–2099, so the
/// non-negative-era simplification holds).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Only ever called with `m` in 1..=12 (validated in `read`).
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Extract one temporal phrase from `query`, resolved against `now_ms`
/// (unix ms, UTC). `None` means the caller searches its original query
/// unmodified — see the module docs for the grammar and boundary rules.
/// Pure and deterministic: same `(query, now_ms)` → same result; no I/O,
/// no clock reads.
pub fn parse_temporal_query(query: &str, now_ms: i64) -> Option<TemporalRewrite> {
    let today = now_ms.div_euclid(DAY_MS) * DAY_MS;
    for (rule, re) in rules() {
        let caps = match re.captures(query) {
            Some(c) => c,
            None => continue,
        };
        // A rule that matched but carries an invalid or inverted date
        // aborts the whole parse (conservative), rather than falling
        // through to a lower-priority partial reading of the same text.
        let (after_ms, before_ms) = match rule {
            Rule::Between => {
                let a = DateSpec::read(&caps, 1)?;
                let b = DateSpec::read(&caps, 4)?;
                if a.start_ms() >= b.end_ms() {
                    return None;
                }
                (Some(a.start_ms()), Some(b.end_ms()))
            }
            Rule::Before => (None, Some(DateSpec::read(&caps, 1)?.start_ms())),
            Rule::After => (Some(DateSpec::read(&caps, 1)?.start_ms()), None),
            Rule::LastNDays => {
                let n: i64 = caps[1].parse().ok()?;
                if n == 0 {
                    return None;
                }
                (Some(today - n * DAY_MS), None)
            }
            Rule::Relative => {
                // Normalize interior whitespace ("last  week") and casing.
                let key = caps[1].to_ascii_lowercase();
                match key
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .as_str()
                {
                    "yesterday" => (Some(today - DAY_MS), Some(today)),
                    "today" => (Some(today), Some(today + DAY_MS)),
                    "last week" => (Some(today - 7 * DAY_MS), None),
                    "last month" => (Some(today - 30 * DAY_MS), None),
                    // The alternation admits nothing else; conservative
                    // None instead of a panic per the crate's no-panic rule.
                    _ => return None,
                }
            }
            Rule::BareDate => {
                let d = DateSpec::read(&caps, 2)?;
                // Reject year-only dates without a temporal preposition (too ambiguous in prose).
                if matches!(d, DateSpec::Year { .. }) && caps.get(1).is_none() {
                    return None;
                }
                (Some(d.start_ms()), Some(d.end_ms()))
            }
        };
        let whole = caps.get(0)?;
        let residual = format!("{} {}", &query[..whole.start()], &query[whole.end()..])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if residual.is_empty() {
            // A pure temporal phrase is not a searchable query.
            return None;
        }
        if !residual.contains(|c: char| c.is_ascii_alphanumeric()) {
            // A residual with no analyzable tokens (e.g. "!!!") would be
            // REJECTED by the engine's tokenizer — before the rewriter
            // existed the date words themselves were searchable, so
            // rewriting here would turn a working query into an error.
            // Pass the original through unrewritten instead.
            return None;
        }
        return Some(TemporalRewrite {
            residual_query: residual,
            after_ms,
            before_ms,
            matched_phrase: whole.as_str().to_string(),
        });
    }
    None
}

/// Resolve an explicit ISO bound to UTC ms. Accepts a date — `2026-08-01`,
/// `2026-08`, or `2026`, resolving to the start of its period — or a UTC
/// datetime `YYYY-MM-DDTHH:MM[:SS]` with an optional trailing `Z`,
/// resolving to that exact instant (non-UTC offsets and fractional seconds
/// are rejected: a bound silently shifted by timezone math would be worse
/// than an error). `None` for anything else. Shared by the MCP
/// `created_after`/`created_before` params and the CLI `--created-*` flags
/// so explicit bounds and the rewriter resolve dates identically; the
/// rewriter itself matches dates only — a datetime inside prose does not
/// trigger a rewrite.
pub fn parse_iso_instant(s: &str) -> Option<i64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"^\s*(19[7-9]\d|20\d{2})(?:-(\d{2})(?:-(\d{2})(?:T(\d{2}):(\d{2})(?::(\d{2}))?Z?)?)?)?\s*$",
        )
        .expect("static temporal pattern")
    });
    let caps = re.captures(s)?;
    let date_ms = DateSpec::read(&caps, 1).map(DateSpec::start_ms)?;
    match (caps.get(4), caps.get(5)) {
        (Some(h), Some(m)) => {
            let (h, m): (i64, i64) = (h.as_str().parse().ok()?, m.as_str().parse().ok()?);
            let sec: i64 = caps.get(6).map_or(Some(0), |x| x.as_str().parse().ok())?;
            if h > 23 || m > 59 || sec > 59 {
                return None;
            }
            Some(date_ms + (h * 3600 + m * 60 + sec) * 1000)
        }
        _ => Some(date_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestCase<'a> = (&'a str, &'a str, Option<i64>, Option<i64>, &'a str);

    /// 2026-08-09T12:00:00Z — a fixed reference "now" (day 20674 since the
    /// epoch; the containing UTC day starts at 1_786_233_600_000). Every
    /// expectation below is hand-derived from these anchors — no clock,
    /// no chrono.
    const NOW_MS: i64 = 1_786_276_800_000;
    const TODAY: i64 = 1_786_233_600_000;
    const DAY: i64 = 86_400_000;

    #[track_caller]
    fn parse(q: &str) -> TemporalRewrite {
        parse_temporal_query(q, NOW_MS).unwrap_or_else(|| panic!("expected a rewrite for {q:?}"))
    }

    fn assert_cases(cases: &[TestCase]) {
        for (query, residual, after, before, phrase) in cases {
            let got = parse(query);
            assert_eq!(got.residual_query, *residual, "residual for {query:?}");
            assert_eq!(got.after_ms, *after, "after_ms for {query:?}");
            assert_eq!(got.before_ms, *before, "before_ms for {query:?}");
            assert_eq!(got.matched_phrase, *phrase, "phrase for {query:?}");
        }
    }

    #[test]
    fn absolute_forms_resolve_to_utc_boundaries() {
        // (query, residual, after_ms, before_ms, matched_phrase)
        // 2026-01-01 = 1_767_225_600_000, 2026-03-02 = 1_772_409_600_000,
        // 2026-08-01 = 1_785_542_400_000, 2026-08-02 = 1_785_628_800_000,
        // 2026-09-01 = 1_788_220_800_000 (all UTC day starts).
        assert_cases(&[
            (
                "decisions before 2026-08-01",
                "decisions",
                None,
                Some(1_785_542_400_000),
                "before 2026-08-01",
            ),
            (
                "decisions until 2026-08",
                "decisions",
                None,
                Some(1_785_542_400_000),
                "until 2026-08",
            ),
            (
                "hnsw work since 2026-01-01",
                "hnsw work",
                Some(1_767_225_600_000),
                None,
                "since 2026-01-01",
            ),
            (
                "hnsw work after 2026",
                "hnsw work",
                Some(1_767_225_600_000),
                None,
                "after 2026",
            ),
            (
                "releases between 2026-01-01 and 2026-03-01",
                "releases",
                Some(1_767_225_600_000),
                Some(1_772_409_600_000),
                "between 2026-01-01 and 2026-03-01",
            ),
            (
                "ci failures in 2026-08",
                "ci failures",
                Some(1_785_542_400_000),
                Some(1_788_220_800_000),
                "in 2026-08",
            ),
            (
                "standup on 2026-08-01",
                "standup",
                Some(1_785_542_400_000),
                Some(1_785_628_800_000),
                "on 2026-08-01",
            ),
        ]);
    }

    #[test]
    fn relative_forms_anchor_on_the_injected_now() {
        assert_cases(&[
            (
                "what shipped yesterday",
                "what shipped",
                Some(TODAY - DAY),
                Some(TODAY),
                "yesterday",
            ),
            (
                "standup notes today",
                "standup notes",
                Some(TODAY),
                Some(TODAY + DAY),
                "today",
            ),
            (
                "bugs last week",
                "bugs",
                Some(TODAY - 7 * DAY),
                None,
                "last week",
            ),
            // Case-insensitive match; matched_phrase keeps original casing.
            (
                "merges Last Month",
                "merges",
                Some(TODAY - 30 * DAY),
                None,
                "Last Month",
            ),
            (
                "deploys last 3 days",
                "deploys",
                Some(TODAY - 3 * DAY),
                None,
                "last 3 days",
            ),
        ]);
    }

    #[test]
    fn unparseable_or_pure_temporal_queries_return_none() {
        for query in [
            "before the v8 migration",                    // no parseable date/anchor
            "last week",                                  // pure temporal → empty residual
            "yesterday",                                  // pure temporal
            "port 8080 config",                           // 4 digits, not a 1970–2099 year
            "releases between 2026-03-01 and 2026-01-01", // inverted range
            "notes before 2026-13-01",                    // calendar-invalid month
            "kind-aware recency prior",                   // nothing temporal at all
        ] {
            assert_eq!(parse_temporal_query(query, NOW_MS), None, "for {query:?}");
        }
    }

    #[test]
    fn residual_strips_the_phrase_without_doubling_spaces() {
        let got = parse("topodb decisions before 2026-08-01 about hnsw");
        assert_eq!(got.residual_query, "topodb decisions about hnsw");
        assert_eq!(got.matched_phrase, "before 2026-08-01");
    }

    #[test]
    fn deterministic_and_pure_under_a_shifted_now() {
        // Relative bounds shift by exactly the now-delta; absolute bounds
        // ignore `now` entirely. Nothing here may read a clock.
        let base = parse_temporal_query("bugs last week", NOW_MS).unwrap();
        let shifted = parse_temporal_query("bugs last week", NOW_MS + 3 * DAY).unwrap();
        assert_eq!(shifted.after_ms, base.after_ms.map(|a| a + 3 * DAY));
        let abs = parse_temporal_query("bugs since 2026-01-01", NOW_MS).unwrap();
        let abs2 = parse_temporal_query("bugs since 2026-01-01", NOW_MS + 3 * DAY).unwrap();
        assert_eq!(abs, abs2);
    }

    #[test]
    fn parse_iso_instant_resolves_start_of_period() {
        assert_eq!(parse_iso_instant("2026-08-01"), Some(1_785_542_400_000));
        assert_eq!(parse_iso_instant("2026-08"), Some(1_785_542_400_000));
        assert_eq!(parse_iso_instant("2026"), Some(1_767_225_600_000));
        for bad in ["not-a-date", "08/01/2026", "", "2026-13-01", "8080"] {
            assert_eq!(parse_iso_instant(bad), None, "for {bad:?}");
        }
    }

    #[test]
    fn parse_iso_instant_accepts_utc_datetimes() {
        // Midnight datetime == the bare date.
        assert_eq!(
            parse_iso_instant("2026-08-01T00:00:00Z"),
            parse_iso_instant("2026-08-01"),
        );
        // 15:30:00 = 55_800_000 ms into the day; Z optional; seconds optional.
        assert_eq!(
            parse_iso_instant("2026-08-01T15:30:00Z"),
            Some(1_785_542_400_000 + 55_800_000),
        );
        assert_eq!(
            parse_iso_instant("2026-08-01T15:30"),
            Some(1_785_542_400_000 + 55_800_000),
        );
        // Non-UTC offsets, fractional seconds, out-of-range fields, and a
        // time without a full date are all rejected, not silently shifted.
        for bad in [
            "2026-08-01T15:30:00+02:00",
            "2026-08-01T15:30:00.5Z",
            "2026-08-01T24:00",
            "2026-08-01T15:61",
            "2026-08T15:30",
        ] {
            assert_eq!(parse_iso_instant(bad), None, "for {bad:?}");
        }
    }

    #[test]
    fn unanalyzable_residual_passes_through_unrewritten() {
        // "!!!" survives phrase-stripping as the residual but contains no
        // tokenizable term — the engine would reject it. The rewriter must
        // step aside so the original query still searches (the date words
        // themselves are searchable terms).
        assert_eq!(parse_temporal_query("!!! since 2026-01-01", NOW_MS), None);
    }

    #[test]
    fn bare_year_requires_preposition() {
        // Bare years without prepositions are rejected to avoid false matches
        // in prose ("the 2026 roadmap" ≠ created in 2026).
        assert_eq!(
            parse_temporal_query("the 2026 roadmap", NOW_MS),
            None,
            "bare year without preposition should not match"
        );

        // Bare years WITH prepositions match correctly.
        // 2026-01-01 = 1_767_225_600_000, 2027-01-01 = 1_798_761_600_000.
        assert_cases(&[
            (
                "decisions in 2026",
                "decisions",
                Some(1_767_225_600_000),
                Some(1_798_761_600_000),
                "in 2026",
            ),
            (
                "shipped during 2026",
                "shipped",
                Some(1_767_225_600_000),
                Some(1_798_761_600_000),
                "during 2026",
            ),
        ]);

        // Bare months and dates still match without prepositions.
        assert_cases(&[
            (
                "incidents 2026-08",
                "incidents",
                Some(1_785_542_400_000),
                Some(1_788_220_800_000),
                "2026-08",
            ),
            (
                "notes 2026-08-01",
                "notes",
                Some(1_785_542_400_000),
                Some(1_785_628_800_000),
                "2026-08-01",
            ),
        ]);
    }
}
