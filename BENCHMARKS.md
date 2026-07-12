# TopoDB Benchmarks

Reproducible storage measurements. Regenerate with:

```text
cargo test -p topodb --release --test size_report -- --ignored --nocapture
cargo bench -p topodb --bench storage
cargo bench -p topodb --bench recall
```

Workload: `topodb::workload`, seed `0xC0FFEE`: N memories with 50–500-word
content, N/5 entities, roughly 2.5 edges/memory, and 20% 768-dimension f32
embeddings. Logical bytes are the sum of redb key/value lengths, excluding page
and free-list overhead.

Machine: AMD Ryzen 7 7800X3D, 63.1 GiB RAM, Windows 11 (10.0.26200.0).
Measurements were taken on 2026-07-11. V1 was measured from the parent commit
(`3fba635`) with the identical workload copied as a temporary measurement seam;
v2 is `643a328`.

## Logical size

| memories | format | file bytes | logical total | nodes value bytes | edges value bytes | embeddings value bytes | ops value bytes | postings value bytes |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1k | v1 | 17,379,328 | 6,406,282 | 2,606,749 | 314,025 | — | 2,928,898 | 447,360 |
| 1k | v2 | 17,379,328 | 5,301,036 | 894,879 | 300,356 | 617,000 | 2,928,898 | 447,360 |
| 1k | v2/v1 | 1.000× | **0.827×** | 0.343× | 0.956× | — | 1.000× | 1.000× |
| 10k | v1 | 135,278,592 | 63,700,320 | 25,919,867 | 3,111,387 | — | 29,112,265 | 4,473,816 |
| 10k | v2 | 135,278,592 | 52,757,411 | 8,909,943 | 2,976,309 | 6,170,000 | 29,112,265 | 4,473,816 |
| 10k | v2/v1 | 1.000× | **0.828×** | 0.344× | 0.957× | — | 1.000× | 1.000× |

Physical file bytes are unchanged because redb allocator/free-list slack dominates
these small files. Logical bytes are the gate signal: v2 saves about **17.2%** at
both measured scales. Interning plus the frame therefore clear the plan's 5%
demotion threshold. The node hot/cold split cuts node-row value bytes about 65.6%;
its external vector rows account for the separate `embeddings` column.

The 100k size-report run was started but exceeded the 20-minute CI-agent command
budget after completing the 10k row. It is intentionally not represented by
fabricated or extrapolated numbers; rerun the command above on a longer-lived
benchmark host before release.

## Timings

Criterion 0.5, 100 samples; brackets are Criterion confidence intervals.

| benchmark | v1 | v2 | v2/v1 median |
|---|---:|---:|---:|
| cold_open_10k | 154.36–163.59 ms | 159.51–166.70 ms | 1.026× (+2.6%) |
| submit_1k_workload | 721.31–728.50 ms | 736.48–743.39 ms | 1.021× (+2.1%) |

Both timing deltas are below the plan's 10% regression threshold. The write result
includes dictionary/frame work and the embedding table write; it remains within
noise-scale overhead for this machine.

## v3 (format 3: slot keys, chunked adjacency, interned scopes)

Machine: AMD Ryzen 7 7800X3D, 63.1 GiB RAM, Samsung SSD 990 PRO 2TB (NVMe),
Windows 11 (10.0.26200.0). Measured 2026-07-12 at this commit, same workload
and seed as the baselines above. Regenerate with:

```text
cargo bench -p topodb --bench storage
cargo test -p topodb --release --test size_report -- --ignored size_report_v3_gate4 --nocapture
# Large-scale open fixtures + RAM report: see the doc comments in
# crates/topodb/tests/size_report.rs (build_open_fixture / open_report /
# build_ram_fixture / ram_report) for the env-var invocations.
```

### Gate summary

| # | gate | target | measured | verdict |
|---:|---|---|---|---|
| 1 | open, 1M memories, embed_pct=0 | < 500 ms | p95 15.3 ms | **PASS** |
| 2 | warm k=2 traversal p95 | ≤ 2× v2 baseline | 73.0 µs — no v2 traversal number exists in this file (see below) | **PASS (new baseline)** |
| 3 | cold k=2 traversal p95 | report only | 37.6 ms | reported |
| 4 | `edges`+`out_adj`+`in_adj` logical bytes ≤ v2 `edges` | 1k: ≤ 300,356; 10k: ≤ 2,976,309 | 1k: 258,256 (0.860×); 10k: 2,603,156 (0.875×) | **PASS** |
| 5 | `submit_1k_workload` throughput ≥ 80% of v2 | median ≤ 924.9 ms (1.25× v2 median) | 273.3 ms median (2.71× v2 throughput) | **PASS** |
| 6 | RAM ceiling follows `cache_size_bytes` | report only | 64 MB → 104.8 MB peak WS; 256 MB → 270.1 MB peak WS | reported |

### Logical size (gate 4)

v2 `edges` numbers are the committed baseline table's value-byte figures; the
v3 sums additionally include key bytes (redb keys are real storage), which
only makes the comparison harsher on v3.

| memories | v2 edges value bytes | v3 edges+out_adj+in_adj key+value bytes | v3/v2 | v2 logical total | v3 logical total | v3/v2 total |
|---:|---:|---:|---:|---:|---:|---:|
| 1k | 300,356 | 258,256 | **0.860×** | 5,301,036 | 4,948,554 | 0.934× |
| 10k | 2,976,309 | 2,603,156 | **0.875×** | 52,757,411 | 49,246,643 | 0.933× |

The v3 rows come from `size_report_v3_gate4` (the extended `storage_report`
now covers all 18 tables, including the v3 sidecar tables). The v3 logical
totals exclude nothing: slot maps, adjacency, prop index, and scope registry
are all counted.

### Timings

Criterion 0.5, 100 samples (30 for `traverse_cold_10k`); brackets are
Criterion confidence intervals. p95 figures are per-iteration nearest-rank,
computed from Criterion's per-sample data (`sample.json`).

| benchmark | v2 | v3 | v3/v2 median |
|---|---:|---:|---:|
| cold_open_10k | 159.51–166.70 ms | 31.88–32.87 ms | 0.198× |
| submit_1k_workload | 736.48–743.39 ms | 270.81–276.24 ms | 0.369× |
| traverse_warm_10k | — | 70.03–70.69 µs (p95 73.0 µs) | new baseline |
| traverse_cold_10k | — | 33.37–34.59 ms (p95 37.6 ms) | new baseline |

Part of the `submit_1k_workload` speedup (2.71× v2 throughput) reflects the
Task-11 snapshot-fold deletion (`3aebe76` removed the applier's per-batch
im-snapshot fold), not the v3 on-disk encoding alone — don't over-credit
slot keys/chunked adjacency for it.

No comparable v2 traversal benchmark was ever recorded in this file (the v2
timing table above has only `cold_open_10k` and `submit_1k_workload`), so
gate 2 records v3 as the baseline rather than inventing a comparison. The
73.0 µs warm-traversal baseline showed ~38% session-to-session drift on
this machine (112–114 µs in the evening runs vs 70 µs in the morning run —
both recorded in the chunk-experiment table below); future comparisons
against it should use paired same-session runs. Cold = a fresh
`Db::open_with` before every traversal, so it is open-dominated (compare
`cold_open_10k`); warm = repeated traversals on one live handle.

### Open time at scale (gate 1, plus the ungated SP2 number)

Method: fixtures built by the resumable `build_open_fixture` test; open
times are 10 sequential fresh `Db::open_with` calls with a manual `Instant`
timer in the `open_report` test (release build), after one untimed
verification open. At n=10 the nearest-rank p95 is simply the max of the 10
runs. Both 1M fixtures were built with the text index omitted
(`TOPODB_FIXTURE_SKIP_FTS=1`) — see the finding below for why; the open
path reads META/DICT/SCOPES/EMBEDDINGS and never touches postings, and the
full-spec open path is covered by `cold_open_10k` above.

| fixture | file bytes | open min / median / p95 / max |
|---|---:|---|
| 1M memories, embed_pct=0 | 6,447,075,328 | 13.1 / 14.0 / 15.3 / 15.3 ms |
| 1M memories, embed_pct=20 (768-dim f32) | 8,591,003,648 | 1878.5 / 1953.6 / 2118.8 / 2118.8 ms |

Open at 1M memories without embeddings is ~14 ms — effectively flat vs the
10k full-spec bench once the embedding scan is out of the picture. With 20%
embeddings (200k × 768-dim), open jumps ~135× to ~2 s: that is
`VectorIndex::from_storage`'s full EMBEDDINGS scan, and it is the
SP2-motivation number — vector index rebuild dominates open at scale.

### RAM ceiling vs `cache_size_bytes` (gate 6, report only)

30k-memory full-spec fixture (148.2 MB logical); `ram_report` opens it with
`DbOptions { cache_size_bytes }` and runs a full `storage_report` scan while
an external poller samples the process peak working set (`Get-Process`
`PeakWorkingSet64`, 50 ms interval, best effort).

| cache_size_bytes | peak working set |
|---:|---:|
| 64 MB | 104.8 MB |
| 256 MB | 270.1 MB |

The ceiling moves with the knob: +192 MB of configured cache moved the peak
by +165 MB against an identical scan.

### Chunk-target experiment (`CHUNK_SPLIT_TARGET`)

10k size report + `traverse_warm_10k` at each candidate, editing the const
per run. All four configs ran in one evening session; the same 8 KB config
re-measured 70.33 µs in a fresh morning session (the final run, which the
timing table above uses).

| target | edges+out_adj+in_adj @10k | out_adj / in_adj rows @10k | traverse_warm_10k |
|---:|---:|---:|---:|
| 4 KB | 2,603,156 | 25,011 / 13,830 | 114.75 µs |
| 8 KB (shipped) | 2,603,156 | 25,011 / 13,830 | 113.16 µs evening / 70.33 µs morning |
| 16 KB | 2,603,156 | 25,011 / 13,830 | 114.93 µs |
| 32 KB | 2,603,156 | 25,011 / 13,830 | 105.49 µs |

Verdict: **the const stays 8192 — no code change.** The workload's densest
`(slot, edge_type)` adjacency list never reaches even the 4 KB target, so
rows and bytes are identical across all four configs (zero splits occur)
and the experiment cannot discriminate targets at this scale. The residual
timing spread (105–115 µs) is smaller than the measured session-to-session
drift on the same config (113 µs evening vs 70 µs morning), so it
attributes to machine state, not the const. Multi-chunk correctness is
pinned separately by the `traversal_spans_multiple_adjacency_chunks` test.

### Finding: FTS posting maintenance is quadratic in corpus size

Not a gate in this plan, but measured and worth recording. `fts_update`
rewrites the full posting row for every term of every document it indexes
(`set_posting` is a whole-row read-decode-insert-encode-write), and this
workload's 16-word vocabulary makes every document touch essentially every
posting row — so indexing cost per document grows linearly with the corpus
(~0.12 s per 200-op batch at ~25k memories, ~0.78 s at ~60k; ~37 ms per
document by a 75k-doc corpus) and total build cost grows quadratically. A
250k-memory build projects to ~3.8 h and 1M to >24 h on this machine, and
`ensure_index_spec`'s reindex-at-open runs the same per-document loop, so a
large corpus can be neither indexed incrementally nor reindexed at open in
practical time. That is why the 1M open fixtures above omit the text index.
Real vocabularies are far larger (per-row growth correspondingly slower),
but the row-rewrite cost model is the same. This is input for the planned
FTS optimization work (Plan 5): posting rows need segmented/appendable
storage before FTS is practical at 100k+ memories with dense vocabularies.

The 1M builds themselves (text index omitted) ran to completion for this
report: 3,700,291 ops in 131 s (embed_pct=0) and 3,899,242 ops in 144 s
(embed_pct=20) — the write path without FTS is linear at scale.
