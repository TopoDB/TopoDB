"""File-level localization metrics. gold names ONE valid fix site; these
metrics reward matching the gold patch's files, not every valid localization."""

def hit_at_k(retrieved: list, gold: set, k: int, mode: str = "any") -> int:
    topk = set(retrieved[:k])
    if mode == "any":
        return 1 if (topk & gold) else 0
    if mode == "all":
        return 1 if gold and gold <= topk else 0
    raise ValueError(f"unknown mode: {mode!r}")

def reciprocal_rank(retrieved: list, gold: set) -> float:
    for i, path in enumerate(retrieved, start=1):
        if path in gold:
            return 1.0 / i
    return 0.0

def aggregate(per_instance: list, ks: list) -> dict:
    n = len(per_instance) or 1
    out = {}
    for k in ks:
        out[f"any@{k}"] = sum(hit_at_k(r["retrieved"], r["gold"], k, "any")
                              for r in per_instance) / n
        out[f"all@{k}"] = sum(hit_at_k(r["retrieved"], r["gold"], k, "all")
                              for r in per_instance) / n
    out["mrr"] = sum(reciprocal_rank(r["retrieved"], r["gold"])
                     for r in per_instance) / n
    return out
