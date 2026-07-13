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

> **Retired in v4** (Task 9, storage-format-v4 plan): postings are now
> chunked (`POSTINGS_CHUNK_TARGET`, `crates/topodb/src/fts.rs`) and the
> append path touches exactly one chunk per document regardless of corpus
> size — this quadratic-total finding no longer holds for the append case.
> The 1M-with-text-index build in the v4 section below did not complete
> within this task's time budget (build cost climbs at continued large
> scale for reasons not fully isolated — see that section's finding), but
> corpora up to 100k memories now build in ~106 s full-spec (vs. this
> finding's ~37 ms/doc and climbing at 75k, i.e. hours), and a genuinely
> new edit-heavy failure mode was found and is recorded separately: an
> old document repeatedly edited to GAIN a term whose covering chunk is
> not the term's last chunk grows that chunk without ever splitting it
> (deliberate, scoped simplification per `fts.rs`'s `mutate_posting_chunk`
> doc comment — the fix scope was the append/fast path only).

## v4 (format 4: clustered vectors, chunked postings)

Machine: AMD Ryzen 7 7800X3D, 63.1 GiB RAM, Windows 11 (10.0.26200.0). Measured
2026-07-12 at this commit, same workload/seed as the baselines above unless
noted. Large fixtures were built on a second NVMe volume (`D:`) after the
default temp volume ran out of free space mid-run — noted because it means
the v3 numbers above (built on the OS volume) and some v4 numbers below
aren't on byte-identical storage hardware; both are NVMe SSDs on the same
machine, and the effect (if any) is expected to be well under the gates'
margins. Regenerate with:

```text
cargo bench -p topodb --bench storage
cargo test -p topodb --release --test size_report -- --ignored fts_linearity_append_report --nocapture
cargo test -p topodb --release --test size_report -- --ignored fts_edit_heavy_report --nocapture
cargo test -p topodb --release --test size_report -- --ignored chunk_target_experiment_report --nocapture
# 1M-scale open fixtures + RAM report: see the doc comments in
# crates/topodb/tests/size_report.rs (build_open_fixture / open_report /
# build_ram_fixture / ram_report) for the env-var invocations.
```

### Gate summary

| # | gate | target | measured | verdict |
|---:|---|---|---|---|
| 1 | open, 1M memories, embed_pct=20 | < 500 ms | p95 11.4 ms (was p95 2118.8 ms in v3) | **PASS** |
| 2 | warm scoped vector search p95, 10k vectors, 768-dim, k=10 | ≤ 2× v2 slab number | p95 15.0 ms — no comparable v2 number exists in BENCHMARKS.md at this dim/scale (see below) | **PASS (new baseline)** |
| 3 | cold scoped vector search p95 | report only | p95 73.0 ms | reported |
| 4 | `get_node` with embedding, v3-vs-v4 | no measurable regression | +280 ns / +8.9% (v3 3167 ns → v4 3448 ns, matched manual harness) | **FINDING — small measurable regression** |
| 5 | RAM ceiling vs `cache_size_bytes`, with embeddings | report only | 64 MB → 85.6 MB peak WS; 256 MB → 252.4 MB peak WS (v3: 104.8 MB / 270.1 MB) | reported |
| 6 | FTS indexing linearity: 100k per-doc ≤ 2× 10k per-doc, AND ≤ 5 ms | both | 10k: (a) 0.53 / (b) 0.66 ms/doc; 100k: (a) 0.84 / (b) 1.10 ms/doc; ratio (a) 1.58× / (b) 1.66× | **PASS** |
| 6b | FTS edit-heavy phase (amendment, reported not gated) | report + growth curve | 696 µs/edit @1k → 1943 µs/edit @12k (2.8× over 12× more edits) | **FINDING — super-linear, as predicted** |
| 7 | open, 1M memories, WITH text index | < 500 ms | not measured at 1M (build exceeded budget); measured at 100k: p95 10.4 ms | **PASS at verified scale; 1M build infeasible in budget (see finding)** |

### Gate 1: open, 1M memories, embed_pct=20 (the v3 gate, vector-fold caveat retired)

Same method as v3's open-time gate (`build_open_fixture`/`open_report`,
`TOPODB_FIXTURE_SKIP_FTS=1` — this gate is about the vector-index rebuild,
not FTS): 10 sequential fresh `Db::open_with` calls, nearest-rank over n=10.

| fixture | file bytes | open min / median / p95 / max |
|---|---:|---|
| 1M memories, embed_pct=20 (768-dim f32), v3 | 8,591,003,648 | 1878.5 / 1953.6 / 2118.8 / 2118.8 ms |
| 1M memories, embed_pct=20 (768-dim f32), v4 | 8,591,003,648 | 8.2 / 8.8 / 11.4 / 11.4 ms |

Identical file size (same workload/seed; the vectors themselves are stored
either way, just in a different table shape), ~186× faster p95 open. This is
exactly SP2's motivation: `VectorIndex::from_storage` — the open-time full
`EMBEDDINGS` scan that used to rebuild the RAM slab — is gone; `search_vector`
now reads `vectors`/`embedding_ref` directly and open touches neither table.

Raw command: `TOPODB_FIXTURE_MEMORIES=1000000 TOPODB_FIXTURE_EMBED_PCT=20
TOPODB_FIXTURE_SKIP_FTS=1 cargo test -p topodb --release --test size_report
-- --ignored build_open_fixture --nocapture` (196.7 s to build, one run,
`complete=true`), then the same env without `TOPODB_BUILD_BUDGET_SECS` against
`open_report`.

### Gate 2/3: scoped vector search, 10k vectors in one scope, 768-dim, k=10

`search_warm_10k_scope`/`search_cold_10k_scope` (`crates/topodb/benches/storage.rs`),
criterion 0.5, n=100 (warm) / n=30 (cold, matching `traverse_cold`'s reduced
sample size for a cold-open-dominated bench). p95 computed from criterion's
per-sample `sample.json`, same nearest-rank method as v3's traversal gates.

| bench | min | median | p95 | max |
|---|---:|---:|---:|---:|
| `search_warm_10k_scope` | 12.0 ms | 13.2 ms | 15.0 ms | 16.9 ms |
| `search_cold_10k_scope` | 61.6 ms | 67.0 ms | 73.0 ms | 73.1 ms |

**No v2 number exists for this configuration.** The only historical vector-search
baseline in this repository is `recall.rs`'s `search_vector_10k_dim32`, a
different bench (dim 32, not 768; the pre-`v1` in-RAM-slab implementation),
last recorded at 574 µs \[565–584\] in the Plan-1 progress ledger (not
BENCHMARKS.md, and superseded three times over by later format changes) — not
a valid comparator for a 768-dim/10k-vector gate. Per the v3 gate-2 precedent
(traversal, no comparable v2 number), this gate records v4 as the new
baseline rather than inventing a comparison.

**Finding, fixed before this report: a degenerate fixture pathologically
inflated the first warm-search measurement to ~950 ms.** The first cut of
`vector_fixture` used one-hot vectors (`v[i % 768] = 1.0`, else 0) — with
only 768 distinct directions spread across 10,000 nodes, most rows land in
large exact-cosine-score tie groups against any fixed query.
`vector_store.rs`'s `push_topk` drains every element tied at the current
k-th-best score on each insert once the heap reaches size `k`; with a
handful of distinct scores total, nearly every one of the 10,000 inserts hit
that drain path, and the bench measured ~950 ms warm / cold correspondingly
inflated. Switching the fixture to dense random vectors (matching
`workload::batches`' own embedding generator, effectively zero exact ties)
dropped warm search to the ~13 ms this report cites — a ~98.6% change,
confirmed by criterion's own before/after comparison
(`change: [-98.624% -98.598% -98.574%] (p = 0.00 < 0.05)`). This was a
fixture defect, not an engine regression, but it's worth flagging forward:
`push_topk`'s tied-group draining is genuinely O(group size) per insert, and
a real corpus with many exact-duplicate or quantized/binary embeddings could
hit a milder version of the same pattern. Not gated here; noted as input for
any future quantization/ANN work.

### Gate 4: `get_node` with an embedding, v3 vs v4

The read path's extra cost: v4's `Storage::load_node`/`read_node_by_slot`
opens `VECTORS`+`EMBEDDING_REF` where v3 opened only `EMBEDDINGS` — the
design spec's "one extra point read." Criterion benches from two separately
compiled binaries aren't comparable to each other, so this delta used a
matched **manual-timing** harness (identical code, `std::time::Instant`,
1,000-iteration warmup + 200,000 timed iterations, `std::hint::black_box`)
run against a v3 checkout (`git worktree add --detach <path> fcbb768`, the
0.0.6/format-v3 release commit) and this branch, same 10k-node/768-dim
fixture shape in both. The harness itself was never committed (throwaway
test file, deleted after use; the worktree was removed after).

| | per-call | 
|---|---:|
| v3 (`fcbb768`) | 3166.8 ns |
| v4 (this branch) | 3447.5 ns |
| delta | +280.7 ns (**+8.9%**) |

The persisted `get_node_embedded` criterion bench (`storage.rs`, same
fixture) independently reports v4 at **3.30–3.39 µs**, median 3.34 µs —
consistent with the manual harness's 3448 ns.

**This is a measurable regression, not "no measurable regression."** It is
small in absolute terms (+280 ns) and matches the design's own accounting
(one extra small-table point read per call), but the gate's literal target
("must not measurably regress it") is not met by a controlled, low-noise
(200k-iteration) measurement showing a reproducible +8.9%. Recorded as a
finding per this task's instructions, not smoothed over.

### Gate 5: RAM ceiling vs `cache_size_bytes`, with embeddings

Same 30k-memory fixture as v3's RAM gate (`build_ram_fixture`,
`WorkloadSpec::default()` — `embed_pct: 20` is already the default, so the v3
number above already included embeddings; this table is the same fixture
shape reopened under v4). `ram_report` run as its own process
(`Start-Process` + `Get-Process -Id <pid>` polling `PeakWorkingSet64` every
50 ms, matching v3's methodology) so each cache setting gets an
uncontaminated peak.

| cache_size_bytes | v3 peak WS | v4 peak WS |
|---:|---:|---:|
| 64 MB | 104.8 MB | 85.6 MB |
| 256 MB | 270.1 MB | 252.4 MB |

The ceiling still moves with the knob (+192 MB configured → +166.8 MB
observed movement, v3 was +165.3 MB — consistent). The v4 numbers run
lower at both settings, consistent with the RAM slab's removal (Task 7):
there is no in-memory vector index to hold alongside the page cache anymore.

### Gate 6: FTS indexing linearity (append phase)

Two independent measurements, both at the final `POSTINGS_CHUNK_TARGET =
4096` (see the chunk-target experiment below for why):

**(a) FTS-isolated harness** (`fts_linearity_append_report`,
`size_report.rs`): a bare `Memory`-only corpus (no entities/edges), so window
timing measures FTS + base node-write cost only. Windowed per-doc rate
(cumulative-elapsed delta ÷ memories in that window):

| corpus checkpoint | window | per-doc |
|---|---|---:|
| 1,000 | 0 → 1,000 | 0.2423 ms |
| 10,000 | 1,000 → 10,000 (9,000 docs) | 0.5307 ms |
| 100,000 | 10,000 → 100,000 (90,000 docs) | 0.8366 ms |

100k/10k ratio: **1.58×** (≤ 2× target). Absolute: **0.84 ms ≤ 5 ms**. **PASS.**
(Full run: 80.4 s total for 100k docs.)

**(b) Full-workload harness** (`build_open_fixture`, entities + `ABOUT`/
`MENTIONS`/`FOLLOWS` edges + text, `TOPODB_FIXTURE_MEMORIES=100000
TOPODB_FIXTURE_SKIP_FTS=0 TOPODB_BUILD_CHUNK=200`) — the same methodology the
original v3 quadratic finding used (`build_open_fixture`'s printed cumulative
`elapsed_secs`), for an apples-to-apples comparison against that 37-ms/doc-at-75k
number:

| corpus checkpoint | window | per-doc |
|---|---|---:|
| ~1,000 | 0 → ~972 memories | 1.03 ms |
| ~10,000 | ~972 → ~10,005 (~9,033 docs) | 0.664 ms |
| 100,000 | ~10,005 → 100,000 (~89,995 docs) | 1.100 ms |

100k/10k ratio: **1.66×** (≤ 2×). Absolute: **1.10 ms ≤ 5 ms**. **PASS.**
Full 100k build (entities+edges+text): **105.9 s** — v3 projected ~3.8 h for
a 250k build and never completed one; v4 builds 100k full-spec, with FTS, in
under two minutes.

Both methodologies agree the gate passes with comfortable margin and that
per-doc cost is roughly flat (not the v3 climb from 25k→75k). A third,
earlier run at the pre-experiment `POSTINGS_CHUNK_TARGET = 8192` produced
consistent PASS numbers too (10k: 0.67 ms, 100k: 0.79 ms, ratio 1.18×) —
included for corroboration, not double-counted in the verdict above.

### Gate 6b: FTS edit-heavy phase (Task 6 reviewer's mandatory amendment)

`fts_edit_heavy_report` (`size_report.rs`): 15,000-document base corpus;
only the high-slot tail (last 500 docs) carries a marker term `"zzmarker"`
at creation (built via the normal append/fast path, so its posting list
starts out correctly split into normal-sized chunks). The edit phase then
adds `"zzmarker"` to the 12,000 low-slot documents that lack it, ascending
by slot, in batches of 200 `SetNodeProps` ops. Every one of those inserts
has a slot below the marker's existing minimum, so `fts.rs`'s covering-chunk
scan routes every single one into the SAME earliest chunk (the first
earlier chunk whose max already exceeds the new — much lower — slot, which
is trivially every earlier chunk once the new slot precedes the whole
existing range) — and that chunk never splits, by the deliberate design
`mutate_posting_chunk`'s doc comment documents ("a covering-chunk insert can
grow a chunk slightly past `POSTINGS_CHUNK_TARGET` without triggering a
split"). At `POSTINGS_CHUNK_TARGET = 4096`:

| edits so far | window per-edit |
|---:|---:|
| 1,000 | 696.3 µs |
| 2,000 | 836.0 µs |
| 4,000 | 1,022.1 µs |
| 8,000 | 1,412.4 µs |
| 12,000 | 1,943.1 µs |

Per-edit cost grows **2.79×** from the 1k checkpoint to the 12k checkpoint —
clearly super-linear, confirming the amendment's prediction. The growth is
milder than a naive O(n) chunk-decode model alone would suggest, because
each edit's `SetNodeProps` also touches the ~16 other, already-stable,
correctly-split terms already in the document (`fts_update` computes
`set_posting` over the union of old/new terms, not just the diff, so every
edit pays a roughly-constant baseline for those in addition to the growing
marker chunk) — that baseline dilutes but does not hide the trend. A
pre-experiment run at `POSTINGS_CHUNK_TARGET = 8192` showed the same shape
(704 µs @1k → 1,431 µs @12k, 2.03×), so this is a mechanism, not a
chunk-target artifact.

**This is a FINDING, not a gate failure** — the deferral (splitting scoped
to the append path only) was adjudicated in Task 6's review, and this
measurement is exactly the input that adjudication asked Task 9 to produce.
It matters for hosts doing bulk retroactive tagging/re-indexing of old
documents (adding a newly-common term across many old rows) at scale; a
single old document occasionally gaining a term is unaffected in practice
(the growth is in the SHARED chunk a hot term's low-slot insertions all
funnel into, not per-document). Recorded here for Plan 5+ to revisit if
edit-heavy workloads become a real access pattern.

### Gate 7: open, 1M memories, WITH text index

**Not measured at the literal 1M scale — build time exceeded this task's
budget, reported honestly per the task's explicit fallback instruction
rather than fabricated or extrapolated.**

Three resumed `build_open_fixture` runs (`TOPODB_FIXTURE_MEMORIES=1000000
TOPODB_FIXTURE_EMBED_PCT=0 TOPODB_FIXTURE_SKIP_FTS=0`, budgeted ~580 s each)
reached 1,223,800 / 3,700,291 ops (≈ 292,500 of 1,000,000 memories) after
~29 minutes of cumulative build time, at a per-op rate that climbed across
the three runs (575,000 ops in run 1 at 0.982 ms/op → next 339,800 ops at
1.71 ms/op → next 309,000 ops at 1.88 ms/op). That climb is NOT reproduced
by either gate-6 methodology above at the 100k scale (both stay in the
0.5–1.1 ms/doc range with a ≤1.66× 10k→100k ratio) — the most likely
explanation is redb/file-growth overhead as the `.redb` file crosses into
multi-GB territory (a mechanism outside `fts.rs`'s chunking logic), not a
reopening of the quadratic postings bug, but this was not isolated further
within budget. **Recorded as an open finding for follow-up**, not
attributed with confidence to any single cause.

**Largest feasible COMPLETE, verified scale: 100,000 memories, full spec
(entities + edges + text), built in 105.9 s** (the same fixture gate 6b's
methodology-(b) run built). Open time against it:

| fixture | file bytes | open min / median / p95 / max |
|---|---:|---|
| 100k memories, full spec incl. text index | 1,078,472,704 | 7.3 / 8.1 / 10.4 / 10.4 ms |

**p95 10.4 ms ≤ 500 ms — PASS at the verified scale.** The open path
structurally never touches `POSTINGS`/`FTS_DOCS`/`FTS_STATS` (confirmed by
source read, `Storage::open_with_options`), so open time is expected to be
independent of both corpus size and whether the text index is populated;
this is corroborated empirically by gate 1's 1M-no-FTS-relevant fixture
(embed_pct=20, skip_fts=1, p95 11.4 ms) landing in the same range as this
100k-with-FTS fixture (p95 10.4 ms) despite an 8.6× larger file and a
populated `POSTINGS` table. The literal "open time at 1M with text index"
number was not directly measured, and this report does not claim it was —
the structural argument plus the 100k empirical point together are the
retirement evidence for BENCHMARKS.md's no-FTS caveat at the verified scale;
full 1M-with-FTS open time is expected, not measured, to also land under
500 ms.

### Chunk-target experiment (`POSTINGS_CHUNK_TARGET`)

`chunk_target_experiment_report` (`size_report.rs`): a 10k-doc append phase
(full FTS-enabled build, one shot) plus a search-latency probe (200 repeated
`search_text(&scopes, "agent", 10)` calls against the same near-universal
term — "agent" hits effectively 100% of this corpus, a worst-case decode-every-chunk
query) plus a scaled-down edit-heavy phase (4,000-doc base / 200-doc marker
tail / 3,000 edits), run once per candidate with the const hand-edited
between runs (no test-only override — same prod/test-parity rationale as
`fts.rs`'s own split tests).

| target | append overall ms/doc | append last-2k ms/doc | search median | search p95 | edit-heavy @3,000 µs/edit |
|---:|---:|---:|---:|---:|---:|
| 4 KB | 0.3855 | 0.4309 | 24.95 ms | 32.10 ms | 444.7 |
| 8 KB (previous default) | 0.7404 | 1.0124 | 31.72 ms | 39.95 ms | 1,164.0 |
| 16 KB | 1.1784 | 1.6430 | 34.93 ms | 40.01 ms | 1,137.5 |
| 32 KB | 1.0382 | 1.7591 | 23.94 ms | 28.49 ms | 758.9 |

**Verdict: `POSTINGS_CHUNK_TARGET` changed from 8192 to 4096 (4 KB).** 4 KB
won or tied-for-best on all three axes at this corpus scale: append cost
roughly half of 8 KB's, edit-heavy cost ~2.6× cheaper than 8 KB's, and
search latency second-best (32 KB's result was marginally lower — within
noise, not a second winner). Smaller covering/last chunks cost less to
decode+re-encode per touch (the dominant cost on both write paths), and this
workload's postings never grow large enough for the extra chunk-count
overhead to dominate reads. A prior, otherwise-identical sweep without the
search-latency probe showed the same append/edit-heavy ordering (4 KB
fastest on both), corroborating the choice. This is the first-ever
measurement of `POSTINGS_CHUNK_TARGET`'s impact — it is a distinct constant
from `adj.rs`'s `CHUNK_SPLIT_TARGET` (adjacency chunking), which v3's
"Chunk-target experiment" table above measured and left at 8 KB; that
finding is about a different data structure and isn't superseded by this
one. Changing the const required re-measuring gates 6 and 6b above at the
new value (done — the numbers in those sections are already at 4 KB), and
the full workspace test suite was re-run and passed after the change (no
test was tuned to a specific byte target).
