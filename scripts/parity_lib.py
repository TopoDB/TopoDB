"""Shared helpers for the parity runners (pytest, CLI)."""


def resolve(x, ids):
    """Replace "#N" back-references (in args or expectations) with ids[N]."""
    if isinstance(x, str) and x.startswith("#"):
        return ids[int(x[1:])]
    if isinstance(x, list):
        return [resolve(v, ids) for v in x]
    if isinstance(x, dict):
        return {k: resolve(v, ids) for k, v in x.items()}
    return x
