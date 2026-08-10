//! Deterministic temporal query rewriting for temporal recall (PR 2 of the
//! semantica inclusions spec). `parse_temporal_query` is pure: `now_ms` is
//! always injected by the caller — nothing in this module may read a clock.
//! Grammar, boundary rules, and the conservative-None contract are pinned
//! by the test table below; the implementation lands in the next task.

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

/// Extract one temporal phrase from `query`, resolved against `now_ms`
/// (unix ms, UTC). `None` means the caller searches its original query
/// unmodified. Pure and deterministic: same `(query, now_ms)` → same
/// result; no I/O, no clock reads.
pub fn parse_temporal_query(query: &str, now_ms: i64) -> Option<TemporalRewrite> {
    let _ = (query, now_ms);
    todo!("implemented in the next task")
}

/// Resolve an explicit ISO date bound (`2026-08-01`, `2026-08`, `2026`) to
/// the UTC ms start of its period; `None` for anything else. Shared by the
/// MCP `created_after`/`created_before` params and the CLI `--created-*`
/// flags — the single place explicit bounds are resolved.
pub fn parse_iso_instant(s: &str) -> Option<i64> {
    let _ = s;
    todo!("implemented in the next task")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-09T12:00:00Z — a fixed reference "now" (day 20674 since the
    /// epoch; the containing UTC day starts at 1_786_233_600_000). Every
    /// expectation below is hand-derived from these anchors — no clock,
    /// no chrono.
    const NOW_MS: i64 = 1_786_276_800_000;
    const TODAY: i64 = 1_786_233_600_000;
    const DAY: i64 = 86_400_000;

    #[track_caller]
    fn parse(q: &str) -> TemporalRewrite {
        parse_temporal_query(q, NOW_MS)
            .unwrap_or_else(|| panic!("expected a rewrite for {q:?}"))
    }

    fn assert_cases(cases: &[(&str, &str, Option<i64>, Option<i64>, &str)]) {
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
            ("decisions before 2026-08-01", "decisions",
             None, Some(1_785_542_400_000), "before 2026-08-01"),
            ("decisions until 2026-08", "decisions",
             None, Some(1_785_542_400_000), "until 2026-08"),
            ("hnsw work since 2026-01-01", "hnsw work",
             Some(1_767_225_600_000), None, "since 2026-01-01"),
            ("hnsw work after 2026", "hnsw work",
             Some(1_767_225_600_000), None, "after 2026"),
            ("releases between 2026-01-01 and 2026-03-01", "releases",
             Some(1_767_225_600_000), Some(1_772_409_600_000),
             "between 2026-01-01 and 2026-03-01"),
            ("ci failures in 2026-08", "ci failures",
             Some(1_785_542_400_000), Some(1_788_220_800_000), "in 2026-08"),
            ("standup on 2026-08-01", "standup",
             Some(1_785_542_400_000), Some(1_785_628_800_000), "on 2026-08-01"),
        ]);
    }

    #[test]
    fn relative_forms_anchor_on_the_injected_now() {
        assert_cases(&[
            ("what shipped yesterday", "what shipped",
             Some(TODAY - DAY), Some(TODAY), "yesterday"),
            ("standup notes today", "standup notes",
             Some(TODAY), Some(TODAY + DAY), "today"),
            ("bugs last week", "bugs", Some(TODAY - 7 * DAY), None, "last week"),
            // Case-insensitive match; matched_phrase keeps original casing.
            ("merges Last Month", "merges",
             Some(TODAY - 30 * DAY), None, "Last Month"),
            ("deploys last 3 days", "deploys",
             Some(TODAY - 3 * DAY), None, "last 3 days"),
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
}
