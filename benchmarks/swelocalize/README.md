# SWE-bench file-localization benchmark

File-level fault localization on SWE-bench-Lite: given a GitHub issue, how often
does TopoDB rank the file(s) the gold patch touches in the top-k?

## Environment

Reuses `../longmemeval/.venv` (TopoDB already installed). Do NOT create a second
venv or rebuild.

Tests (no network, no model download):
    cd benchmarks/swelocalize && PYTHONPATH=. ../longmemeval/.venv/bin/python -m pytest tests/ -q

## Real run (manual; needs network for the HF dataset + git clones)

    pip install datasets sentence-transformers    # into the shared venv
    cd benchmarks/swelocalize
    PYTHONPATH=. ../longmemeval/.venv/bin/python -m swe.run --limit 30      # subset-first
    PYTHONPATH=. ../longmemeval/.venv/bin/python -m swe.run                 # full 300

Results + fairness manifest are written to `results/swelocalize.json`. Each run
clears `.cache/dbs` to ensure metrics reflect one clean pass; `.cache/repos`
clones are preserved across runs (clones are expensive and reusable).

## Legs

- **text** — BM25 over file content (path prepended, so path tokens match).
- **vector** — host-computed MiniLM, mean-pooled over file chunks (a weak code
  embedder — a code-specific model is a documented future A/B).
- **hybrid** — text+vector RRF.
- **graph** — hybrid + TopoDB's `recall` graph leg (PPR over the `imports`
  edges, seeded from the top lexical/vector hits). This is the same leg that was
  *neutral* on LongMemEval; the open question is whether a real import graph
  makes it non-neutral.

## Honest limits

- Metric rewards matching the **gold patch's** files — one valid fix site, not
  every place the bug could be fixed. `any@k` is a proxy for correct
  localization.
- Gold files that the patch **creates** do not exist at `base_commit`, so they
  can never be retrieved and those instances score 0 on every leg. This is a
  uniform floor across legs (so cross-leg comparison is unaffected), and each
  run reports the count as `unretrievable` (`full` = all gold absent, `any` =
  at least one gold file absent).
- Import resolution is best-effort `ast`: dynamic imports, `__init__`
  re-exports, star-imports, and conditional imports may be missed.
- Vector leg is mean-pooled per file; max-pool via per-chunk nodes is a future
  refinement.
- Any `--limit` subset is recorded in the manifest — never a silent cap.
