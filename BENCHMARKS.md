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
