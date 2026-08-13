# LongMemEval-S Recall Harness

Measures TopoDB's session-level Recall@k on LongMemEval-S. See the design at
`docs/superpowers/specs/2026-08-13-longmemeval-recall-harness-design.md`.

## Setup
1. Build the Python bindings (from repo root):
   `maturin develop -m crates/topodb-py/Cargo.toml`
2. Install deps: `pip install -r benchmarks/longmemeval/requirements.txt`
3. Place `longmemeval_s.json` under `benchmarks/longmemeval/data/`
   (gitignored). See `lme/data.py:download` for the source.

## Run (from benchmarks/longmemeval/)
Smoke: `python -m lme.run --data data/longmemeval_s.json --limit 5`
Full:  `python -m lme.run --data data/longmemeval_s.json`
Report: `python -m lme.report results/<file>.json`

## Test
`pytest` (from benchmarks/longmemeval/)
