"""Embodied-memory spike orchestrator (design §4.4).

Deterministic end-to-end harness: generate a seeded synthetic world, ingest it
into a temporary TopoDB scope through :class:`emb.store.EmbodiedStore`, answer
every generator query through the engine-only query layer, and score each answer
against the generator's ground-truth. Reports per-type accuracy plus an overall
number.

Run with ``python -m emb.run`` (from ``benchmarks/embodied``). No TopoDB import
here — everything goes through the store adapter, so a passing run genuinely
exercises the engine surface the higher layers pin.
"""

from __future__ import annotations

import os
import tempfile

import emb.world
import emb.store
import emb.ingest
import emb.queries

# Fixed default seed keeps the whole run reproducible (design §4.4).
SEED = 42


def _score(got, expected) -> bool:
    """Exact match for scalar (str) answers, set-equality for list answers."""
    if isinstance(expected, list):
        got_list = got if isinstance(got, list) else ([] if got is None else [got])
        return set(got_list) == set(expected)
    return got == expected


def run(seed: int = SEED) -> float:
    """Ingest a seeded world, answer its queries, print an accuracy report, and
    return the overall accuracy."""
    world = emb.world.generate_world(seed)

    tmp_dir = tempfile.mkdtemp(prefix="emb-spike-")
    db_path = os.path.join(tmp_dir, "emb.redb")

    store = emb.store.EmbodiedStore(db_path)
    store.open()

    id_map = emb.ingest.ingest_world(store, world)

    # Per-type tallies: {type: [correct, total]}.
    tally: dict[str, list] = {}
    for q in world.queries:
        qtype = q["type"]
        got = emb.queries.answer(store, world, id_map, q)
        ok = _score(got, q["answer"])
        bucket = tally.setdefault(qtype, [0, 0])
        bucket[0] += int(ok)
        bucket[1] += 1

    # Ordered rows: known query taxonomy first, then any extras, deterministically.
    ordered = [t for t in emb.world.QUERY_TYPES if t in tally]
    ordered += sorted(t for t in tally if t not in emb.world.QUERY_TYPES)

    total_correct = sum(b[0] for b in tally.values())
    total_n = sum(b[1] for b in tally.values())

    width = max((len(t) for t in ordered), default=len("type"))
    width = max(width, len("type"))

    print(f"{'type':<{width}}  {'accuracy':>8}  {'n':>3}")
    print(f"{'-' * width}  {'-' * 8}  {'-' * 3}")
    for t in ordered:
        correct, n = tally[t]
        acc = correct / n if n else 0.0
        print(f"{t:<{width}}  {acc:>8.3f}  {n:>3}")
    print(f"{'-' * width}  {'-' * 8}  {'-' * 3}")

    overall = total_correct / total_n if total_n else 0.0
    print(f"overall accuracy: {overall:.3f}  ({total_correct}/{total_n})")

    return overall


if __name__ == "__main__":
    run()
