//! Explicit size report: `cargo test -p topodb --release --test size_report -- --ignored --nocapture`.
//! v3 additions (Task 13 / BENCHMARKS.md v3): `size_report_v3_gate4` (the
//! `edges`+`out_adj`+`in_adj` size-gate numbers at v2-comparable scales),
//! `build_open_fixture` / `open_report` (the large-scale open-time gate,
//! split into a resumable time-budgeted build step and a separate
//! manual-timer measurement step so no single command has to outlive a
//! CI-agent command budget), and `build_ram_fixture` / `ram_report` (the
//! external-process RAM gate). See each test's doc comment for its exact
//! invocation.
use topodb::workload::{batches, WorkloadSpec};
use topodb::{Db, DbOptions, IndexSpec, PropIndex};
fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    }
}

/// Shared body for `size_report`/`size_report_v3_gate4`: build each scale in
/// `scales` fresh, reopen, print every `storage_report` row, and (v3 addition)
/// the `edges`+`out_adj`+`in_adj` combined logical-byte figure the size gate
/// compares against v2's `edges` column in the committed BENCHMARKS.md table.
fn run_size_report(scales: &[usize]) {
    for &memories in scales {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.redb");
        let db = Db::open_with(&path, spec()).unwrap();
        for batch in batches(&WorkloadSpec {
            memories,
            ..Default::default()
        }) {
            db.submit(batch).unwrap();
        }
        drop(db);
        let db = Db::open_with(&path, spec()).unwrap();
        let report = db.storage_report().unwrap();
        let file = std::fs::metadata(&path).unwrap().len();
        println!("\n== {memories} memories == file: {file}");
        let mut total = 0;
        for r in &report {
            println!("{} {} {} {}", r.table, r.rows, r.key_bytes, r.value_bytes);
            total += r.key_bytes + r.value_bytes;
        }
        println!("logical total: {total}");
        let edges = report.iter().find(|r| r.table == "edges").unwrap();
        let out_adj = report.iter().find(|r| r.table == "out_adj").unwrap();
        let in_adj = report.iter().find(|r| r.table == "in_adj").unwrap();
        let edge_family = edges.key_bytes
            + edges.value_bytes
            + out_adj.key_bytes
            + out_adj.value_bytes
            + in_adj.key_bytes
            + in_adj.value_bytes;
        println!("edges+out_adj+in_adj logical bytes: {edge_family}");
    }
}

#[test]
#[ignore]
fn size_report() {
    run_size_report(&[1_000usize, 10_000, 100_000]);
}

/// v3 size gate (BENCHMARKS.md gate 4): `edges`+`out_adj`+`in_adj` logical
/// bytes at the two scales the committed v2 baseline table has an `edges`
/// column for (1k, 10k) — deliberately excludes the slow 100k scale, which
/// `size_report` above already documents as exceeding the CI-agent command
/// budget and isn't needed for this comparison.
#[test]
#[ignore]
fn size_report_v3_gate4() {
    run_size_report(&[1_000usize, 10_000]);
}

/// Times `iters` sequential fresh `Db::open_with` calls against an
/// already-built `path`, sorted ascending so callers can read off
/// min/median/p95/max by index (nearest-rank).
fn timed_opens(
    path: &std::path::Path,
    open_spec: &IndexSpec,
    iters: usize,
) -> Vec<std::time::Duration> {
    let mut out = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = std::time::Instant::now();
        let db = Db::open_with(path, open_spec.clone()).unwrap();
        out.push(start.elapsed());
        drop(db);
    }
    out.sort();
    out
}

fn print_open_stats(times: &[std::time::Duration]) {
    for (i, t) in times.iter().enumerate() {
        println!("  open[{i}] = {:.1} ms", t.as_secs_f64() * 1000.0);
    }
    let p95_idx = (times.len() * 95 / 100).min(times.len() - 1);
    println!(
        "open_min_ms={:.1} open_median_ms={:.1} open_p95_ms={:.1} open_max_ms={:.1}",
        times[0].as_secs_f64() * 1000.0,
        times[times.len() / 2].as_secs_f64() * 1000.0,
        times[p95_idx].as_secs_f64() * 1000.0,
        times[times.len() - 1].as_secs_f64() * 1000.0,
    );
}

/// Env-var knob with a default, for the fixture tests below (all values are
/// plain integers).
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// The spec the open-time fixture is built AND reopened with. With
/// `TOPODB_FIXTURE_SKIP_FTS=1`, the text index is omitted (the equality
/// index stays): the workload's 16-word vocabulary makes FTS maintenance
/// rewrite every posting row per document — O(corpus) per doc, O(corpus²)
/// total, measured at ~37 ms/doc by a 75k-doc corpus — so a million-memory
/// text index cannot be built (or reindexed: `ensure_index_spec` runs the
/// same per-doc `fts_update`) in practical time. The open path never reads
/// POSTINGS/FTS_DOCS/FTS_STATS (see `Storage::open_with`), so omitting the
/// text index changes open time only through the identical META spec
/// compare; `cold_open_10k` in `benches/storage.rs` covers the full-spec
/// open path. BOTH `build_open_fixture` and `open_report` must run with the
/// same value: reopening a skip-FTS fixture with the full spec would
/// trigger that impractical full reindex at open.
fn fixture_spec(skip_fts: bool) -> IndexSpec {
    let mut s = spec();
    if skip_fts {
        s.text.clear();
    }
    s
}

/// Fixed (non-tempdir) path for an open-time fixture, keyed by its workload
/// shape (and whether the text index was skipped, so the two fixture kinds
/// never collide) so the no-embeddings gate fixture and the with-embeddings
/// SP2 fixture coexist. Deliberately persistent across processes: the build
/// is resumable and the measurement runs in a later command. Not cleaned up
/// automatically.
fn open_fixture_path(memories: usize, embed_pct: usize, skip_fts: bool) -> std::path::PathBuf {
    let fts = if skip_fts { "nofts" } else { "fts" };
    std::env::temp_dir().join(format!("topodb_v3_open_{memories}_{embed_pct}_{fts}.redb"))
}

/// v3 open-time gate, step 1 (resumable, time-budgeted build):
/// `cargo test -p topodb --release --test size_report -- --ignored build_open_fixture --nocapture`
/// with `TOPODB_FIXTURE_MEMORIES` (default 250_000), `TOPODB_FIXTURE_EMBED_PCT`
/// (default 0), `TOPODB_FIXTURE_SKIP_FTS` (see `fixture_spec` — required at
/// large scales), `TOPODB_BUILD_BUDGET_SECS` (default 420), and
/// `TOPODB_BUILD_CHUNK` (ops per submit, default 5_000) in the environment.
/// Submits the workload's op stream to the fixed-path fixture until the
/// stream is exhausted or the budget elapses; rerunning the identical
/// command resumes from the durable op log (`current_seq` counts committed
/// ops, and any prefix of the stream is a valid resume point because chunks
/// commit atomically and this test re-chunks from the raw op offset). Run
/// repeatedly until it prints `complete=true`.
///
/// Why the chunk knob exists: the workload's 16-word vocabulary makes every
/// submit rewrite essentially every FTS posting row, and rows grow with the
/// corpus — so build cost is quadratic in memories divided by ops-per-submit.
/// The fixture's final logical state is submit-size-invariant (one OPS row
/// per op either way; NODES/EDGES/adjacency/postings are functions of the
/// final corpus, not of batch boundaries), so building with large chunks
/// changes only build time, never what `open_report` measures. The
/// canonical `size_report`/`size_report_v3_gate4` numbers keep the
/// workload's own 200-op batches.
#[test]
#[ignore]
fn build_open_fixture() {
    let memories = env_usize("TOPODB_FIXTURE_MEMORIES", 250_000);
    let embed_pct = env_usize("TOPODB_FIXTURE_EMBED_PCT", 0);
    let skip_fts = env_usize("TOPODB_FIXTURE_SKIP_FTS", 0) == 1;
    let budget = std::time::Duration::from_secs(env_usize("TOPODB_BUILD_BUDGET_SECS", 420) as u64);
    let chunk = env_usize("TOPODB_BUILD_CHUNK", 5_000);
    let path = open_fixture_path(memories, embed_pct, skip_fts);
    let ops: Vec<_> = batches(&WorkloadSpec {
        memories,
        embed_pct: embed_pct as u8,
        ..Default::default()
    })
    .into_iter()
    .flatten()
    .collect();
    let db = Db::open_with(&path, fixture_spec(skip_fts)).unwrap();
    let done = db.current_seq().unwrap() as usize;
    assert!(
        done <= ops.len(),
        "fixture has more ops than this workload — wrong TOPODB_FIXTURE_* env?"
    );
    let start = std::time::Instant::now();
    let mut submitted = done;
    for slice in ops[done..].chunks(chunk) {
        if start.elapsed() > budget {
            break;
        }
        db.submit(slice.to_vec()).unwrap();
        submitted += slice.len();
        println!(
            "  ops {submitted}/{} elapsed_secs={:.0}",
            ops.len(),
            start.elapsed().as_secs_f64()
        );
    }
    println!(
        "memories={memories} embed_pct={embed_pct} skip_fts={skip_fts} ops={submitted}/{} run_secs={:.1} complete={}",
        ops.len(),
        start.elapsed().as_secs_f64(),
        submitted == ops.len()
    );
}

/// v3 open-time gate, step 2 (measurement — gate 1 of BENCHMARKS.md v3):
/// `cargo test -p topodb --release --test size_report -- --ignored open_report --nocapture`
/// with the same `TOPODB_FIXTURE_MEMORIES`/`TOPODB_FIXTURE_EMBED_PCT` as the
/// completed `build_open_fixture` runs. Verifies the fixture is fully built
/// (refusing to time a partial one), then times 10 sequential fresh
/// `Db::open_with` calls with a manual `Instant` timer and prints
/// min/median/p95/max — the method BENCHMARKS.md's open rows cite.
#[test]
#[ignore]
fn open_report() {
    let memories = env_usize("TOPODB_FIXTURE_MEMORIES", 250_000);
    let embed_pct = env_usize("TOPODB_FIXTURE_EMBED_PCT", 0);
    let skip_fts = env_usize("TOPODB_FIXTURE_SKIP_FTS", 0) == 1;
    let path = open_fixture_path(memories, embed_pct, skip_fts);
    let open_spec = fixture_spec(skip_fts);
    let expected_ops: u64 = batches(&WorkloadSpec {
        memories,
        embed_pct: embed_pct as u8,
        ..Default::default()
    })
    .iter()
    .map(|b| b.len() as u64)
    .sum();
    // Untimed verification open (doubles as warm-up): a partial fixture must
    // fail loudly here, never get silently timed as if it were the real one.
    {
        let db = Db::open_with(&path, open_spec.clone()).unwrap();
        assert_eq!(
            db.current_seq().unwrap(),
            expected_ops,
            "fixture incomplete — rerun build_open_fixture until complete=true"
        );
    }
    println!("memories={memories} embed_pct={embed_pct} skip_fts={skip_fts}");
    println!("file_bytes={}", std::fs::metadata(&path).unwrap().len());
    print_open_stats(&timed_opens(&path, &open_spec, 10));
}

/// Fixed (non-tempdir) path for the RAM gate's shared fixture — built once by
/// `build_ram_fixture`, then reopened by separate `ram_report` process
/// invocations (one per `cache_size_bytes`) so each gets an accurate,
/// uncontaminated peak working-set reading. Not cleaned up automatically.
fn ram_fixture_path() -> std::path::PathBuf {
    std::env::temp_dir().join("topodb_v3_ram_fixture.redb")
}

/// v3 RAM gate, step 1: `cargo test -p topodb --release --test size_report -- --ignored build_ram_fixture --nocapture`.
/// Builds the 30k-memory fixture `ram_report` reopens per cache setting. 30k
/// (~150+ MB logical, per `size_report_v3_gate4`'s 10k/1k scaling) sits
/// comfortably between the gate's two `cache_size_bytes` settings (64 MB,
/// 256 MB), so the working-set ceiling has room to visibly move with the
/// knob instead of both settings trivially fitting the whole file in cache.
#[test]
#[ignore]
fn build_ram_fixture() {
    let path = ram_fixture_path();
    if path.exists() {
        std::fs::remove_file(&path).unwrap();
    }
    let db = Db::open_with(&path, spec()).unwrap();
    for batch in batches(&WorkloadSpec {
        memories: 30_000,
        ..Default::default()
    }) {
        db.submit(batch).unwrap();
    }
    drop(db);
    println!("built {}", path.display());
}

/// v3 RAM gate, step 2: run as its own OS process (not sharing a process
/// with `build_ram_fixture` or another `ram_report`) so `Get-Process`
/// peak-working-set sampling reflects only this open+scan.
/// `TOPODB_RAM_CACHE_MB` (default 64) sets `DbOptions::cache_size_bytes`.
/// `cargo test -p topodb --release --test size_report -- --ignored ram_report --nocapture`
#[test]
#[ignore]
fn ram_report() {
    let cache_mb: usize = std::env::var("TOPODB_RAM_CACHE_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let path = ram_fixture_path();
    let opts = DbOptions {
        cache_size_bytes: Some(cache_mb * 1024 * 1024),
    };
    let db = Db::open_with_options(&path, spec(), opts).unwrap();
    let report = db.storage_report().unwrap();
    let total: u64 = report.iter().map(|r| r.key_bytes + r.value_bytes).sum();
    println!("cache_mb={cache_mb} logical_total={total}");
    // Holds the process alive so the external harness has a window to poll
    // `Get-Process -Id <pid>).PeakWorkingSet64` before the process exits.
    std::thread::sleep(std::time::Duration::from_secs(3));
}
