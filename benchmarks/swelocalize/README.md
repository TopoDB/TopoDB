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
- **Index-once-per-repo approximation.** Building the text + vector index over
  a whole repo is the per-instance cost floor (~125s just for the BM25 text
  index on django's ~2,600 files, plus the HNSW build). Since a repo's instances
  share almost all files, each repo is indexed **once** at a reference checkout
  (the first instance's `base_commit`, recorded per repo in the manifest as
  `reference_commits`) and every instance of that repo is scored against it. The
  trade-off: an instance whose own `base_commit` differs from the reference sees
  slightly drifted file *contents* (paths are almost always stable, so file-level
  localization is largely unaffected). A gold file absent from the reference
  corpus — whether patch-created or dropped by drift — is counted in
  `unretrievable` (see below).
- Gold files absent from the reference corpus (the patch **creates** them, or
  drift/rename dropped them) can never be retrieved and those instances score 0
  on every leg. This is a uniform floor across legs (so cross-leg comparison is
  unaffected), and each run reports the count as `unretrievable` (`full` = all
  gold absent, `any` = at least one gold file absent).
- Import resolution is best-effort `ast`: dynamic imports, `__init__`
  re-exports, star-imports, and conditional imports may be missed.
- Vector leg is mean-pooled per file; max-pool via per-chunk nodes is a future
  refinement.
- Any `--limit` subset is recorded in the manifest — never a silent cap.
