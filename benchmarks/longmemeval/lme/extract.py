"""Deterministic, dependency-free entity extraction for LongMemEval scopes.

Implements the entity-sharing signal described in the LongMemEval harness
spec §5.1 ("shared-entity induced cross-session memory pairs") and the
determinism / no-heavy-deps requirements of spec §10.

The idea: within the set of sessions that make up one question's scope, find
proper-noun surface forms (capitalized tokens and contiguous capitalized
n-gram "proper-noun phrases") that recur across *different* sessions. Two
sessions that share such a surface form are an induced cross-session memory
pair — the signal the recall harness uses to link memories that a naive
per-session store would keep apart.

Everything here is pure Python string manipulation: no spaCy, no external
model, no network, no heavy dependencies. Output is fully deterministic given
identical input (stable sorting throughout; no reliance on set/dict iteration
order for anything user-visible).

--------------------------------------------------------------------------
Public interface (imported by lme/store.py — keep these stable):

    extract_shared_entities(pairs) -> dict[str, set[str]]
        `pairs` is an iterable of (session_id, content) tuples for one
        question's scope. Returns a *deterministically ordered* mapping
        (an insertion-ordered dict whose keys are sorted) of

            entity_surface (lowercased, normalized)  ->  set of session_ids

        containing ONLY surface forms that occur in >= 2 DISTINCT sessions.

    cross_session_pairs(pairs_or_mapping) -> list[tuple[str, str, str]]
        Accepts either the same `pairs` iterable OR a mapping already
        produced by extract_shared_entities(). Returns the induced
        cross-session memory pairs as a sorted list of

            (entity_surface, session_id_a, session_id_b)

        where session_id_a < session_id_b (the two members are always from
        DIFFERENT sessions and share `entity_surface`). Deterministic sorted
        order.
--------------------------------------------------------------------------
"""

from __future__ import annotations

import re
from itertools import combinations
from typing import Dict, Iterable, List, Mapping, Set, Tuple

__all__ = ["extract_shared_entities", "cross_session_pairs"]

# ---------------------------------------------------------------------------
# Stopwords: common words that are frequently capitalized (sentence starts,
# headings, politeness) but are not entities. Compared against the fully
# lowercased surface form, so single-token forms whose normalization lands
# here are dropped. Kept intentionally small and general per spec §5.1.
# ---------------------------------------------------------------------------
STOPWORDS: frozenset = frozenset(
    {
        "a", "an", "and", "are", "as", "at", "be", "been", "but", "by",
        "can", "could", "did", "do", "does", "for", "from", "had", "has",
        "have", "he", "her", "here", "hers", "him", "his", "how", "i",
        "if", "in", "into", "is", "it", "its", "just", "me", "my", "no",
        "not", "of", "off", "oh", "ok", "okay", "on", "once", "one", "or",
        "our", "ours", "out", "over", "she", "so", "some", "than", "that",
        "the", "their", "theirs", "them", "then", "there", "these", "they",
        "this", "those", "thus", "to", "too", "up", "us", "was", "we",
        "well", "were", "what", "when", "where", "which", "while", "who",
        "whom", "why", "will", "with", "would", "yes", "you", "your",
        "yours",
        # weekday / month common capitalized words that are rarely useful as
        # standalone entities for cross-session linkage
        "monday", "tuesday", "wednesday", "thursday", "friday", "saturday",
        "sunday", "today", "tomorrow", "yesterday",
        "january", "february", "march", "april", "may", "june", "july",
        "august", "september", "october", "november", "december",
    }
)

# Corpus-level truecasing filter. A genuine proper noun ("United", "Zara") is
# capitalized in nearly ALL of its occurrences; a common word that only looks
# like a proper noun because it leads a sentence ("Use", "Consider", "Make") is
# capitalized in a minority of its occurrences and lowercase elsewhere. Requiring
# a high capitalization ratio across the whole scope removes the sentence-initial
# common-word noise that the per-sentence artifact filter cannot catch (bullets,
# colons, and headings all create pseudo "mid-sentence" capitalized common
# words). Data-driven and dependency-free.
CAP_RATIO_MIN = 0.85
# Ignore ratios computed from too few observations (noise); a token seen only
# once is kept (a rare real name would otherwise be dropped) — the >=2-session
# cross-session requirement already filters truly incidental forms.
MIN_RATIO_OBS = 3

# A token is a maximal run of letters/digits with internal apostrophes or
# hyphens allowed (e.g. "O'Brien", "well-known").
_TOKEN_RE = re.compile(r"[A-Za-z0-9]+(?:['\-][A-Za-z0-9]+)*")

# Contraction suffixes ("I'm", "that's", "don't", "we'll"): these normalize to
# apostrophe forms that are always capitalized when they lead a sentence, so the
# cap-ratio does not catch them. Drop any surface form carrying one.
_CONTRACTION_RE = re.compile(r"'(s|ll|m|ve|d|re|t)$", re.IGNORECASE)

# Sentence boundary: run of sentence-final punctuation, or a newline. Used
# only to identify which token starts a sentence so we can drop artifacts
# that are capitalized *only* because they lead a sentence.
_SENT_SPLIT_RE = re.compile(r"[.!?]+|\n+")


def _normalize_text(text: str) -> str:
    """Normalize whitespace deterministically without altering casing.

    Casing is preserved here because the capitalization signal is what the
    proper-noun heuristic keys on; lowercasing happens per surface form later.
    """
    if text is None:
        return ""
    # Collapse all runs of whitespace to single spaces, but keep newlines as
    # explicit sentence boundaries first by mapping them to a period-space.
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    return text


def _is_capitalized(token: str) -> bool:
    """True if the token's first alphabetic char is uppercase (proper-noun
    candidate). Purely orthographic — no dictionary lookup."""
    for ch in token:
        if ch.isalpha():
            return ch.isupper()
    return False  # all-digit tokens are not capitalization candidates


def _normalize_surface(surface: str) -> str:
    """Lowercase-normalize a surface form and collapse internal whitespace."""
    return " ".join(surface.split()).lower()


def _iter_surface_forms(content: str) -> Iterable[str]:
    """Yield normalized proper-noun surface forms found in `content`.

    A surface form is a maximal contiguous run of capitalized tokens (a
    proper-noun phrase). Each occurrence that begins at a sentence-initial
    position is treated as a *potential artifact*: it only counts if the
    same surface form also occurs somewhere that is NOT sentence-initial.
    This deterministically filters tokens capitalized solely because they
    start a sentence.
    """
    content = _normalize_text(content)

    # Track, per normalized surface form, whether we ever saw it in a
    # non-sentence-initial position (a genuine proper-noun occurrence) and
    # whether we saw it at all (initial-only occurrence).
    seen_noninitial: Set[str] = set()
    seen_any: Set[str] = set()

    for sentence in _SENT_SPLIT_RE.split(content):
        tokens = list(_TOKEN_RE.finditer(sentence))
        if not tokens:
            continue

        i = 0
        n = len(tokens)
        while i < n:
            if not _is_capitalized(tokens[i].group(0)):
                i += 1
                continue

            # Extend a maximal run of consecutive capitalized tokens.
            j = i
            while j < n and _is_capitalized(tokens[j].group(0)):
                j += 1

            run_tokens = [t.group(0) for t in tokens[i:j]]
            run_is_sentence_initial = i == 0

            # Emit the full phrase and each individual token as surface forms.
            # A single leading common word (the classic sentence-start
            # artifact) is thereby only rescued if it recurs mid-sentence.
            forms: List[str] = [" ".join(run_tokens)]
            if len(run_tokens) > 1:
                forms.extend(run_tokens)

            for form in forms:
                norm = _normalize_surface(form)
                if not norm:
                    continue
                seen_any.add(norm)
                # A multi-token phrase whose run does not start the sentence,
                # OR any token past the first, is non-initial. For the run
                # starting at position 0, only the very first token is the
                # artifact; later tokens in that run are non-initial.
                if not run_is_sentence_initial:
                    seen_noninitial.add(norm)
                else:
                    # run starts sentence: the whole-phrase form and the
                    # first single token are initial artifacts; any single
                    # token other than the first is non-initial.
                    if form != run_tokens[0] and form != " ".join(run_tokens):
                        seen_noninitial.add(norm)

            i = j

    # Accept a surface form if it has at least one non-initial occurrence
    # (proving it is a genuine proper noun rather than a sentence-start
    # artifact) and it is not a stopword.
    for norm in sorted(seen_noninitial):
        if norm in STOPWORDS:
            continue
        yield norm


def _truecased_tokens(contents: Iterable[str]) -> Set[str]:
    """Corpus-level allowlist of lowercased tokens that behave like proper nouns.

    A token qualifies if it is capitalized in at least `CAP_RATIO_MIN` of its
    occurrences across the whole scope (or is too rare to judge, `< MIN_RATIO_OBS`
    observations, in which case we keep it rather than discard a rare real name).
    Stopwords and contraction forms never qualify. Deterministic — pure counts.
    """
    total: Dict[str, int] = {}
    capd: Dict[str, int] = {}
    for content in contents:
        for m in _TOKEN_RE.finditer(_normalize_text(content or "")):
            tok = m.group(0)
            low = tok.lower()
            total[low] = total.get(low, 0) + 1
            if tok[:1].isupper():
                capd[low] = capd.get(low, 0) + 1
    allow: Set[str] = set()
    for low, n in total.items():
        # Single letters (list markers "A."/"B.", grades "C", initials) are not
        # entities and connect unrelated sessions; require >= 2 characters.
        if len(low) < 2 or low in STOPWORDS or _CONTRACTION_RE.search(low):
            continue
        ratio = capd.get(low, 0) / n
        if n < MIN_RATIO_OBS or ratio >= CAP_RATIO_MIN:
            allow.add(low)
    return allow


def _is_proper_surface(surface: str, allow: Set[str]) -> bool:
    """A surface form survives iff it carries no contraction and every one of its
    whitespace-separated tokens is in the truecased allowlist."""
    if _CONTRACTION_RE.search(surface):
        return False
    toks = surface.split()
    return bool(toks) and all(t in allow for t in toks)


def extract_shared_entities(
    pairs: Iterable[Tuple[str, str]],
) -> Dict[str, Set[str]]:
    """Return a deterministic sorted mapping of shared entity surface forms.

    `pairs`: iterable of (session_id, content) for one question's scope.

    Returns an insertion-ordered dict (keys sorted) mapping each normalized
    entity surface form to the set of session_ids it appears in, keeping ONLY
    surface forms that (a) pass the corpus-level truecasing filter (genuine
    proper nouns, not sentence-initial common words) and (b) occur in >= 2
    DISTINCT sessions.
    """
    pairs = list(pairs)
    allow = _truecased_tokens(content for _, content in pairs)

    surface_to_sessions: Dict[str, Set[str]] = {}
    for session_id, content in pairs:
        sid = str(session_id)
        # De-duplicate within a session: presence, not frequency, matters.
        for surface in set(_iter_surface_forms(content or "")):
            if _is_proper_surface(surface, allow):
                surface_to_sessions.setdefault(surface, set()).add(sid)

    # Keep only cross-session surface forms and emit in deterministic order.
    result: Dict[str, Set[str]] = {}
    for surface in sorted(surface_to_sessions):
        sessions = surface_to_sessions[surface]
        if len(sessions) >= 2:
            result[surface] = sessions
    return result


def cross_session_pairs(
    pairs_or_mapping: "Iterable[Tuple[str, str]] | Mapping[str, Set[str]]",
) -> List[Tuple[str, str, str]]:
    """Return induced cross-session memory pairs in deterministic sorted order.

    Accepts either the raw `pairs` iterable of (session_id, content) OR a
    mapping already produced by extract_shared_entities().

    Returns a sorted list of (entity_surface, session_id_a, session_id_b)
    tuples where session_id_a < session_id_b. Every pair's two members are
    from DIFFERENT sessions and share `entity_surface`.
    """
    if isinstance(pairs_or_mapping, Mapping):
        mapping: Mapping[str, Set[str]] = pairs_or_mapping
    else:
        mapping = extract_shared_entities(pairs_or_mapping)

    induced: List[Tuple[str, str, str]] = []
    for surface in sorted(mapping):
        sessions = sorted(str(s) for s in mapping[surface])
        for a, b in combinations(sessions, 2):
            # combinations over a sorted list already yields a < b and a != b.
            induced.append((surface, a, b))

    induced.sort()
    return induced
