//! Pure duplicate/supersession classification shared by every write front
//! end. No `Db` access, no I/O — callers do their own candidate search
//! (`topodb-mcp`'s `near_duplicates`, `topodb-cli`'s text-only remember
//! probe) and hand pairs of content strings here to score and classify.

use std::collections::{BTreeSet, HashSet};

/// Cosine-similarity floor for surfacing a semantic near-duplicate.
///
/// Calibrated against the default model (bge-small-en-v1.5): the same fact in
/// different words scores ~0.83, an unrelated fact well under 0.5, so 0.80
/// catches near-duplicates while staying clear of the noise floor. Not set
/// higher because the model compresses "same fact" to ~0.83, not 0.95+; and
/// every hit is only advisory, so a borderline false positive costs the caller
/// a glance, not data.
pub const NEAR_DUP_THRESHOLD: f32 = 0.80;
/// How many near-duplicates to surface at most — enough to notice a redundancy
/// without burying the caller.
pub const NEAR_DUP_K: usize = 3;

/// BM25 candidate window for the write-time advisory; the scan is exhaustive.
pub const TEXT_NEAR_DUP_CANDIDATES: usize = NEAR_DUP_K + 5;

/// Cosine floor below `NEAR_DUP_THRESHOLD` at which a pair is still surfaced, but
/// only as a weaker `"possible"` candidate. Measurement showed genuine reworded
/// duplicates can sit as low as ~0.70 — and merely-related facts sit right there
/// too (0.69), so there is NO floor that catches the former without the latter.
/// Set just under that overlap (0.68) so borderline restatements are SURFACED
/// for the caller (an LLM, a native entailment judge) to confirm, rather than
/// silently dropped; the `"possible"` band is the warning that these need a look,
/// not an automatic merge. Widening recall at the cost of precision is the right
/// trade for an advisory tool where a human/agent makes the final call.
pub const NEAR_DUP_REVIEW: f32 = 0.68;

/// Text containment floor for text-based near-duplicate fallback when the embedder
/// isn't Ready. CONTAINMENT = |∩| / min(|A|,|B|) scores candidates by how completely
/// one fact is contained in another — the canonical restatement shape. Floor 0.7
/// catches the canonical pair ("Vega stores data in postgres" / "Vega now stores data
/// in sqlite for embedded mode" = 5/6 ≈ 0.833) while leaving unrelated disjoint facts
/// near 0; a short fact fully contained in a longer restatement scores 1.0 exactly
/// (supersession/rewording). See [`dup_band`] for confidence levels.
pub const TEXT_NEAR_DUP_CONTAINMENT: f64 = 0.7;

/// Floor on the SMALLER token set's size before a text-mode containment score
/// may claim the "likely" band. Whitespace tokens include stopwords, so a short
/// memory's set is trivially contained in any longer memory that happens to
/// mention the same words — containment 1.0 from 3 shared tokens is weak
/// evidence, not strong. 6 keeps the calibrated canonical pair ("Vega stores
/// its data in postgres", 6 tokens, 0.833 → likely) exactly at its band.
pub const TEXT_BAND_MIN_TOKENS: usize = 6;

/// Words that retract or flip a token — the signal that separates a
/// *contradiction* (one fact superseding another) from a *duplicate*: sentence
/// embeddings score "X is A" and "X is now B, not A" as MORE similar than a
/// genuine restatement, so cosine alone can never tell them apart, but the cue
/// can. Split by which way they govern:
/// - PRE cues govern the tokens AFTER them ("not redb", "no longer windows").
const DUP_FWD_CUES: &[&str] = &[
    "not", "never", "no", "longer", "instead", "without", "rather", "over", "versus", "vs",
    "replaced", "replaces", "removed", "remove",
];
/// - POST cues govern the token immediately BEFORE them ("windows dropped",
///   "redb backend removed"). Without these, a post-nominal negation reads as an
///   assertion, so "windows dropped" and "no longer windows" — which AGREE —
///   would be mislabeled a contradiction.
const DUP_BWD_CUES: &[&str] = &[
    "dropped",
    "drops",
    "removed",
    "remove",
    "gone",
    "deprecated",
    "retired",
    "stopped",
    "killed",
    "disabled",
    "discontinued",
    "replaced",
    "replaces",
];

/// Function words (and a few high-frequency verbs) dropped before comparing
/// content tokens, so overlap reflects the salient nouns, not scaffolding.
const DUP_STOP: &[&str] = &[
    "a", "an", "at", "the", "of", "to", "in", "on", "for", "its", "it", "is", "are", "as", "by",
    "with", "and", "or", "now", "only", "both", "this", "that", "using", "use", "uses", "chose",
    "runs", "run", "was", "were", "be", "been", "their", "them", "people", "up",
];

/// How many CONTENT tokens after a PRE cue are treated as governed by it —
/// stopwords and cues in between don't consume window slots, and the window
/// never crosses a clause boundary. (POST cues take just the one content token
/// before them, to avoid over-negating.)
const DUP_FWD_WINDOW: usize = 4;

/// Tokenize a string into a set of lowercase tokens (whitespace-split).
pub fn tokens(s: &str) -> BTreeSet<String> {
    s.split_whitespace().map(|t| t.to_lowercase()).collect()
}

/// Containment similarity of two precomputed token sets: |∩| / min(|A|,|B|).
/// Returns 1.0 if both sets are empty. Returns 0.0 if exactly one set is empty
/// (no overlap is possible, so containment is a deliberate 0.0 rather than NaN).
pub fn containment_of_sets(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let intersection = a.intersection(b).count() as f64;
    let min_len = (a.len().min(b.len())) as f64;

    intersection / min_len
}

/// Confidence band for a near-dup similarity: `"likely"` at/above the strong
/// floor ([`NEAR_DUP_THRESHOLD`]), `"possible"` in the review band below it.
pub fn dup_band(similarity: f32) -> &'static str {
    if similarity >= NEAR_DUP_THRESHOLD {
        "likely"
    } else {
        "possible"
    }
}

/// Band for a TEXT-mode (containment) score: [`dup_band`]'s cosine-derived
/// cutoffs, except capped at "possible" when the smaller token set is under
/// [`TEXT_BAND_MIN_TOKENS`] — the score is real (the pair still surfaces), but
/// small-set containment can't justify "likely".
pub fn text_dup_band(containment: f64, min_set_len: usize) -> &'static str {
    if min_set_len < TEXT_BAND_MIN_TOKENS {
        "possible"
    } else {
        dup_band(containment as f32)
    }
}

fn dup_is_cue(w: &str) -> bool {
    DUP_FWD_CUES.contains(&w) || DUP_BWD_CUES.contains(&w)
}

fn dup_singularize(t: &str) -> &str {
    if t.len() > 3 && t.ends_with('s') {
        &t[..t.len() - 1]
    } else {
        t
    }
}

/// Tokenize `s` (lowercased alphanumeric runs) into (asserted, negated) content
/// sets: `negated` = content tokens governed by a cue; `asserted` = every other
/// content token. Stopwords and cues are dropped. Cue windows are
/// CLAUSE-BOUNDED (a cue never governs tokens past a `.,;:!?()` boundary — "no
/// longer runs on windows, only ubuntu" must not negate "ubuntu") and the
/// forward window counts CONTENT tokens only, so filler like "point load tests
/// at the" can't exhaust it before the salient object.
fn dup_analyze(s: &str) -> (HashSet<String>, HashSet<String>) {
    let lower = s.to_lowercase();
    let is_stop = |w: &str| DUP_STOP.contains(&w);
    let mut negated: HashSet<String> = HashSet::new();
    let mut content: HashSet<String> = HashSet::new();
    for clause in lower.split(['.', ',', ';', ':', '!', '?', '(', ')']) {
        let toks: Vec<&str> = clause
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        for (i, t) in toks.iter().enumerate() {
            if DUP_FWD_CUES.contains(t) {
                let mut taken = 0usize;
                for w in &toks[i + 1..] {
                    if taken == DUP_FWD_WINDOW {
                        break;
                    }
                    if is_stop(w) || dup_is_cue(w) {
                        continue;
                    }
                    negated.insert(dup_singularize(w).to_string());
                    taken += 1;
                }
            }
            if DUP_BWD_CUES.contains(t) {
                // Nearest content token before the cue: "windows dropped" -> windows.
                for w in toks[..i].iter().rev() {
                    if is_stop(w) || dup_is_cue(w) {
                        continue;
                    }
                    negated.insert(dup_singularize(w).to_string());
                    break;
                }
            }
        }
        content.extend(
            toks.iter()
                .filter(|w| !is_stop(w) && !dup_is_cue(w))
                .map(|w| dup_singularize(w).to_string()),
        );
    }
    let asserted = content.difference(&negated).cloned().collect();
    (asserted, negated)
}

/// True when the two contents read as a CONTRADICTION rather than a restatement:
/// one asserts a salient token the other negates. Cheap and deterministic — the
/// hint that a high-similarity pair is a supersession (retire the stale one), not
/// a duplicate (merge them). Calibrated in the module tests against a labeled
/// battery.
pub fn is_supersession(a: &str, b: &str) -> bool {
    let (a_assert, a_neg) = dup_analyze(a);
    let (b_assert, b_neg) = dup_analyze(b);
    a_neg.intersection(&b_assert).next().is_some() || b_neg.intersection(&a_assert).next().is_some()
}

/// `"supersession"` when the pair contradicts (see [`is_supersession`]), else
/// `"duplicate"`.
pub fn dup_relation(a: &str, b: &str) -> &'static str {
    if is_supersession(a, b) {
        "supersession"
    } else {
        "duplicate"
    }
}

#[cfg(test)]
mod dup_classify_tests {
    use super::{containment_of_sets, dup_band, dup_relation, is_supersession, text_dup_band};

    // Labeled battery from the calibration experiment (raw cosine can't separate
    // these — the negation cue must). SAME/UNRELATED => "duplicate" relation,
    // CONTRADICT => "supersession".
    #[test]
    fn supersession_detector_separates_contradictions_from_restatements() {
        let same = [
            ("The team chose redb as TopoDB's storage engine for its single-file ACID guarantees",
             "TopoDB persists its data in the redb embedded key-value database"),
            ("TopoDB uses redb as its storage backend", "The storage engine behind TopoDB is redb"),
            ("Drew prefers Colima over Docker Desktop",
             "Drew runs containers on Colima instead of Docker Desktop"),
            ("CI runs fmt, clippy, and tests on ubuntu and windows",
             "The CI pipeline executes formatting, linting, and the test suite on both ubuntu and windows runners"),
            ("the auth service issues JWT tokens to sign in users",
             "auth uses JSON Web Tokens to authenticate and log people in"),
            // Post-nominal negation that AGREES with a pre-nominal one — both say
            // Windows is gone — must read as a duplicate, not a contradiction.
            ("CI runs only on ubuntu (windows dropped)",
             "CI no longer runs on windows, only ubuntu"),
        ];
        let contradict = [
            (
                "TopoDB stores its data in redb",
                "TopoDB now stores its data in sled, not redb",
            ),
            (
                "the auth service issues JWT tokens",
                "the auth service now issues opaque session tokens, not JWTs",
            ),
            (
                "CI runs on ubuntu and windows",
                "CI no longer runs on windows, only ubuntu",
            ),
            // Post-nominal negation ("... was removed") must fire too.
            (
                "the redb backend is used for storage",
                "the redb backend was removed",
            ),
            // Field-test canonical probe: sentence-initial "never" whose scope
            // must reach past filler ("point load tests at the") to the salient
            // object — requires the content-token window, not the raw one.
            (
                "use the staging db",
                "never point load tests at the staging db",
            ),
        ];
        for (a, b) in same {
            assert!(
                !is_supersession(a, b),
                "should read as a duplicate: {a:?} / {b:?}"
            );
            assert_eq!(dup_relation(a, b), "duplicate");
        }
        for (a, b) in contradict {
            assert!(
                is_supersession(a, b),
                "should read as a supersession: {a:?} / {b:?}"
            );
            assert_eq!(dup_relation(a, b), "supersession");
        }
    }

    #[test]
    fn is_supersession_is_symmetric() {
        let a = "TopoDB stores its data in redb";
        let b = "TopoDB now stores its data in sled, not redb";
        assert_eq!(is_supersession(a, b), is_supersession(b, a));
    }

    #[test]
    fn band_splits_at_the_strong_floor() {
        assert_eq!(dup_band(0.95), "likely");
        assert_eq!(dup_band(0.80), "likely");
        assert_eq!(dup_band(0.799), "possible");
        assert_eq!(dup_band(0.70), "possible");
    }

    #[test]
    fn text_band_caps_small_sets_at_possible() {
        // Stopword-driven containment 1.0 on a tiny set must not read "likely".
        assert_eq!(text_dup_band(1.0, 3), "possible");
        assert_eq!(text_dup_band(1.0, 5), "possible");
        // At the boundary (the calibrated canonical pair has 6 tokens) the
        // normal cosine-derived cutoffs apply unchanged.
        assert_eq!(text_dup_band(0.8333, 6), "likely");
        assert_eq!(text_dup_band(0.75, 6), "possible");
    }

    #[test]
    fn containment_empty_set_rules() {
        use std::collections::BTreeSet;
        let empty: BTreeSet<String> = BTreeSet::new();
        let full: BTreeSet<String> = ["staging".to_string()].into_iter().collect();
        // Both empty: identical, containment 1.0 (existing, deliberate).
        assert_eq!(containment_of_sets(&empty, &empty), 1.0);
        // Exactly one empty: no overlap is possible — 0.0, NOT NaN (which would
        // silently fail every >= floor comparison).
        assert_eq!(containment_of_sets(&empty, &full), 0.0);
        assert_eq!(containment_of_sets(&full, &empty), 0.0);
    }
}
