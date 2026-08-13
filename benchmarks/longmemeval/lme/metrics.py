"""Pure session-level Recall@k math. No I/O, no engine."""
from dataclasses import dataclass


def _distinct_prefix(ranked_sessions: list[str], k: int) -> list[str]:
    """First k distinct session ids, in first-seen order."""
    seen: list[str] = []
    for s in ranked_sessions:
        if s not in seen:
            seen.append(s)
            if len(seen) == k:
                break
    return seen


def recall_at_k(ranked_sessions: list[str], gold: set[str], k: int) -> float:
    top = set(_distinct_prefix(ranked_sessions, k))
    return 1.0 if gold & top else 0.0


def coverage_at_k(ranked_sessions: list[str], gold: set[str], k: int) -> float:
    if not gold:
        return 0.0
    top = set(_distinct_prefix(ranked_sessions, k))
    return len(gold & top) / len(gold)


@dataclass
class QScore:
    question_type: str
    is_abstention: bool
    gold: set[str]
    ranked_sessions: list[str]


def _mean(vals: list[float]) -> float:
    return sum(vals) / len(vals) if vals else 0.0


def _block(scores: list["QScore"], ks: list[int]) -> dict:
    out = {"n": len(scores)}
    for k in ks:
        out[f"recall@{k}"] = _mean([recall_at_k(s.ranked_sessions, s.gold, k) for s in scores])
        out[f"coverage@{k}"] = _mean([coverage_at_k(s.ranked_sessions, s.gold, k) for s in scores])
    return out


def aggregate(scores: list["QScore"], ks: list[int]) -> dict:
    graded = [s for s in scores if not s.is_abstention]
    per_type: dict[str, dict] = {}
    types = sorted({s.question_type for s in graded})
    for t in types:
        per_type[t] = _block([s for s in graded if s.question_type == t], ks)
    return {
        "n": len(graded),
        "n_abstention": sum(1 for s in scores if s.is_abstention),
        "overall": _block(graded, ks),
        "per_type": per_type,
    }
