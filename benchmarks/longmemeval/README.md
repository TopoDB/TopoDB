# LongMemEval-S Recall Harness

Measures TopoDB's session-level Recall@k on LongMemEval-S. See the design at
`docs/superpowers/specs/2026-08-13-longmemeval-recall-harness-design.md`.

## Setup
1. Build the Python bindings into a venv (from repo root):
   `maturin develop -m crates/topodb-py/Cargo.toml`
2. Install deps: `pip install -r benchmarks/longmemeval/requirements.txt`
   (this pulls `sentence-transformers`/`torch` — only needed for a real run,
   not for `pytest`, which uses injected fake encoders).
3. Fetch the dataset (~278 MB JSON, not gitignored-committed):
   ```
   mkdir -p benchmarks/longmemeval/data
   curl -L -o benchmarks/longmemeval/data/longmemeval_s.json \
     https://huggingface.co/datasets/xiaowu0162/longmemeval/resolve/main/longmemeval_s
   ```
   (`lme.data.download` prints this pointer too.)

## Run (from benchmarks/longmemeval/)
Smoke: `python -m lme.run --data data/longmemeval_s.json --granularity session --limit 5`
Full:  `python -m lme.run --data data/longmemeval_s.json`
Report: `python -m lme.report results/<file>.json`

## Test
`pytest` (from benchmarks/longmemeval/) — no dataset or model download required.

## Interpreting results
- Rows are `<granularity>:<leg>`; columns are session-level Recall@k (a hit =
  a gold evidence session appears among the first k DISTINCT retrieved sessions).
- `hybrid − vector` is the RRF fusion delta. Per-type rows show WHERE we win/lose.
- `coverage@k` (in the JSON) matters only for multi-evidence question types.
- Runs are deterministic given the dataset and model: per-question scopes are
  derived from the question index and embeddings are cached, so two runs produce
  identical numbers. (`--seed` is recorded in the manifest for provenance; it is
  not currently wired into any RNG.)

## Caveats (see spec §9)
- **The graph/PPR leg is inert here** — sessions are ingested as plain memories
  with no entities/edges, so `graph_boost` contributes nothing and `hybrid`
  measures text+vector RRF fusion. Activating the graph leg would require
  entity/relation extraction over the sessions (a future lever, and exactly the
  LLM-graph step some competitors perform).
- **This measures ranking, not TopoDB's default embedder.** All vectors are
  host-computed MiniLM (`all-MiniLM-L6-v2`, to match competitors and to run on a
  machine whose ONNX embedder is inert). A separate run could use TopoDB's own
  embedder on suitable hardware.
- **Recall bounds, but does not equal, end-to-end answer accuracy** — there is no
  reader/judge model; abstention questions are excluded from recall and counted
  separately.
