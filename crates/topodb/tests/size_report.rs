//! Explicit size report: `cargo test -p topodb --release --test size_report -- --ignored --nocapture`.
//! v3 additions (Task 13 / BENCHMARKS.md v3): `size_report_v3_gate4` (the
//! `edges`+`out_adj`+`in_adj` size-gate numbers at v2-comparable scales),
//! `build_open_fixture` / `open_report` (the large-scale open-time gate,
//! split into a resumable time-budgeted build step and a separate
//! manual-timer measurement step so no single command has to outlive a
//! CI-agent command budget), and `build_ram_fixture` / `ram_report` (the
//! external-process RAM gate). See each test's doc comment for its exact
//! invocation.
//!
//! v4 additions (Task 9 / BENCHMARKS.md v4): `fts_linearity_append_report`
//! (gate 6's append-phase per-doc cost at 1k/10k/100k) and
//! `fts_edit_heavy_report` (the Task-6-reviewer-mandated edit-heavy phase —
//! re-indexing EXISTING low-slot documents to gain a term, the case
//! `mutate_posting_chunk`'s doc comment flags as never splitting). Both are
//! resumable-in-spirit (single self-contained runs well under the CI-agent
//! command budget once the append path is O(1)/doc) rather than
//! checkpoint/resume like `build_open_fixture`, since neither needs it at
//! the scales gate 6 specifies. `chunk_target_experiment_report` is the
//! smaller, faster combined append+edit-heavy harness re-run once per
//! `POSTINGS_CHUNK_TARGET` candidate (see BENCHMARKS.md's chunk-target
//! experiment table for the four recorded runs).
use std::collections::BTreeMap;
use topodb::workload::{batches, WorkloadSpec};
use topodb::{
    Db, DbOptions, IndexSpec, NodeId, Op, PropIndex, PropValue, Props, Scope, ScopeId, ScopeSet,
};
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

/// Deterministic 16-word-vocabulary sentence generator, mirroring
/// `topodb::workload`'s private `SplitMix64`/`sentence` shape (that module
/// doesn't expose either, and the FTS harnesses below need per-document
/// control `workload::batches` doesn't offer — which docs carry which extra
/// marker terms, and at what slot).
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}
const FTS_WORDS: [&str; 16] = [
    "agent", "memory", "graph", "scope", "recall", "vector", "search", "index", "temporal", "edge",
    "node", "label", "batch", "snapshot", "project", "decision",
];
fn fts_sentence(r: &mut Rng) -> String {
    (0..50 + r.below(451))
        .map(|_| FTS_WORDS[r.below(FTS_WORDS.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Full-spec `IndexSpec` (equality + text) with only the "Memory"/"content"
/// text declaration — the FTS harnesses below don't create "Entity" nodes,
/// so the equality half of `spec()` is unused but harmless to keep for
/// consistency with every other bench/report in this file.
fn fts_spec() -> IndexSpec {
    spec()
}

/// Task 9 gate 6, append phase: per-doc FTS indexing cost at 1k/10k/100k
/// corpus size (`--release`; the fixed on-disk chunk target, no test-only
/// override — same prod/test-parity rationale as `fts.rs`'s split tests).
/// Entity-free by design (a bare `Memory`-only corpus) so window timing
/// below measures FTS + base write cost only, not the workload's
/// entity-creation prefix.
///
/// `cargo test -p topodb --release --test size_report -- --ignored fts_linearity_append_report --nocapture`
#[test]
#[ignore]
fn fts_linearity_append_report() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fts_linearity.redb");
    let db = Db::open_with(&path, fts_spec()).unwrap();
    let scope = Scope::Id(ScopeId::from_u128(1));
    let total = 100_000usize;
    let checkpoints = [1_000usize, 10_000, 100_000];

    let mut r = Rng(0xC0FFEE);
    let mut memories_done = 0usize;
    let mut window_start_memories = 0usize;
    let mut window_start_time = std::time::Instant::now();
    let mut next_checkpoint = 0usize;
    let mut batch: Vec<Op> = Vec::with_capacity(200);
    let mut batch_n = 0usize;

    for i in 0..total {
        let content = fts_sentence(&mut r);
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(content));
        batch.push(Op::CreateNode {
            id: NodeId::new(),
            scope,
            label: "Memory".into(),
            props,
        });
        batch_n += 1;
        let is_last = i + 1 == total;
        if batch_n == 200 || is_last {
            let start = std::time::Instant::now();
            db.submit(std::mem::take(&mut batch)).unwrap();
            let elapsed = start.elapsed();
            memories_done += batch_n;
            println!(
                "memories={memories_done}/{total} batch_n={batch_n} batch_elapsed_ms={:.2}",
                elapsed.as_secs_f64() * 1000.0
            );
            batch_n = 0;
            if next_checkpoint < checkpoints.len() && memories_done >= checkpoints[next_checkpoint]
            {
                let window_memories = memories_done - window_start_memories;
                let window_elapsed = window_start_time.elapsed();
                let per_doc_ms = window_elapsed.as_secs_f64() * 1000.0 / window_memories as f64;
                println!(
                    "CHECKPOINT corpus={} window_memories={window_memories} window_elapsed_s={:.2} per_doc_ms={:.4}",
                    checkpoints[next_checkpoint],
                    window_elapsed.as_secs_f64(),
                    per_doc_ms
                );
                next_checkpoint += 1;
                window_start_memories = memories_done;
                window_start_time = std::time::Instant::now();
            }
        }
    }
}

/// Task 9 gate 6 amendment (edit-heavy phase, Task 6 reviewer): re-indexes
/// EXISTING low-slot documents to GAIN a term that's already hot in the
/// corpus, the case `fts.rs`'s `mutate_posting_chunk` doc comment names as
/// deliberately unsplit ("a covering-chunk insert can grow a chunk slightly
/// past `POSTINGS_CHUNK_TARGET` without triggering a split").
///
/// Setup: `BASE_DOCS` documents built low-slot-first; only the HIGH-slot
/// tail (`MARKER_DOCS`) carries the marker term `"zzmarker"` at creation, so
/// its posting list is seeded entirely via the fast/last-chunk append path
/// (correctly split, normal-sized chunks). The edit phase then adds
/// `"zzmarker"` to the LOW-slot docs that lack it, one batch at a time, in
/// ascending slot order — every one of those inserts has a slot BELOW the
/// marker's existing minimum, so `set_posting` routes every single one into
/// the marker's EARLIEST chunk (`fts.rs`'s covering-chunk scan picks the
/// first earlier chunk whose max already reaches the new slot, which is
/// trivially true for the first chunk once the new slot is below the
/// term's entire existing range) — that chunk never splits, by design, so
/// this is the one scenario that can grow a chunk unboundedly. Report:
/// per-edit cost across increasing edit counts, so growth (or its absence)
/// shows as a curve.
///
/// `cargo test -p topodb --release --test size_report -- --ignored fts_edit_heavy_report --nocapture`
#[test]
#[ignore]
fn fts_edit_heavy_report() {
    const BASE_DOCS: usize = 15_000;
    const MARKER_DOCS: usize = 500;
    const EDIT_BATCH: usize = 200;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fts_edit_heavy.redb");
    let db = Db::open_with(&path, fts_spec()).unwrap();
    let scope_id = ScopeId::from_u128(1);
    let scope = Scope::Id(scope_id);
    let scopes = ScopeSet::of(&[scope_id]);

    let mut r = Rng(0xBADC0DE);
    let mut ids = Vec::with_capacity(BASE_DOCS);
    let mut contents = Vec::with_capacity(BASE_DOCS);
    for _ in 0..BASE_DOCS {
        ids.push(NodeId::new());
    }

    let build_start = std::time::Instant::now();
    let mut batch: Vec<Op> = Vec::with_capacity(200);
    for (i, &id) in ids.iter().enumerate() {
        let mut content = fts_sentence(&mut r);
        if i >= BASE_DOCS - MARKER_DOCS {
            content.push_str(" zzmarker");
        }
        contents.push(content.clone());
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(content));
        batch.push(Op::CreateNode {
            id,
            scope,
            label: "Memory".into(),
            props,
        });
        if batch.len() == 200 || i + 1 == BASE_DOCS {
            db.submit(std::mem::take(&mut batch)).unwrap();
        }
    }
    println!(
        "base_docs={BASE_DOCS} marker_docs={MARKER_DOCS} build_elapsed_s={:.2}",
        build_start.elapsed().as_secs_f64()
    );

    // Sanity: the marker must be present in exactly the high-slot tail and
    // absent everywhere else before any edit runs.
    let before = db.search_text(&scopes, "zzmarker", BASE_DOCS).unwrap();
    println!("marker_hits_before_edits={}", before.len());
    assert_eq!(
        before.len(),
        MARKER_DOCS,
        "fixture setup: marker must start present in exactly MARKER_DOCS documents"
    );

    let edit_candidates = BASE_DOCS - MARKER_DOCS;
    let checkpoints: Vec<usize> = [1_000usize, 2_000, 4_000, 8_000, 12_000]
        .into_iter()
        .filter(|&c| c <= edit_candidates)
        .collect();
    let total_edits = *checkpoints.last().unwrap();

    let mut edits_done = 0usize;
    let mut window_start = 0usize;
    let mut window_time = std::time::Instant::now();
    let mut next_checkpoint = 0usize;
    let mut idx = 0usize;
    while edits_done < total_edits {
        let batch_end = (idx + EDIT_BATCH).min(total_edits);
        let mut ops = Vec::with_capacity(batch_end - idx);
        for doc_i in idx..batch_end {
            let mut props: BTreeMap<String, Option<PropValue>> = BTreeMap::new();
            let new_content = format!("{} zzmarker", contents[doc_i]);
            props.insert("content".to_string(), Some(PropValue::Str(new_content)));
            ops.push(Op::SetNodeProps {
                id: ids[doc_i],
                props,
            });
        }
        let n = batch_end - idx;
        let start = std::time::Instant::now();
        db.submit(ops).unwrap();
        let elapsed = start.elapsed();
        edits_done += n;
        idx = batch_end;
        println!(
            "edits={edits_done}/{total_edits} batch_n={n} batch_elapsed_ms={:.3} per_edit_us={:.1}",
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1e6 / n as f64
        );
        if next_checkpoint < checkpoints.len() && edits_done >= checkpoints[next_checkpoint] {
            let window_n = edits_done - window_start;
            let window_elapsed = window_time.elapsed();
            println!(
                "CHECKPOINT edits={} window_n={window_n} window_elapsed_ms={:.2} per_edit_us={:.1}",
                checkpoints[next_checkpoint],
                window_elapsed.as_secs_f64() * 1000.0,
                window_elapsed.as_secs_f64() * 1e6 / window_n as f64
            );
            next_checkpoint += 1;
            window_start = edits_done;
            window_time = std::time::Instant::now();
        }
    }

    let after = db.search_text(&scopes, "zzmarker", BASE_DOCS).unwrap();
    println!("marker_hits_after_edits={}", after.len());
    assert_eq!(
        after.len(),
        MARKER_DOCS + total_edits,
        "every edited doc must now carry the marker term"
    );
}

/// Postings chunk-target experiment (BENCHMARKS.md): a smaller, faster
/// combined append+edit-heavy harness re-run once per `POSTINGS_CHUNK_TARGET`
/// candidate (the const is edited by hand between runs — see
/// `crates/topodb/src/fts.rs`). 10k-doc append corpus (matches the v3
/// chunk-target experiment's scale) plus a 4k-doc/3k-edit scaled-down
/// edit-heavy phase, so all four candidates fit in one session without a
/// 100k-doc rebuild per target.
///
/// `cargo test -p topodb --release --test size_report -- --ignored chunk_target_experiment_report --nocapture`
#[test]
#[ignore]
fn chunk_target_experiment_report() {
    // --- Append phase: 10k docs, one shot, overall + last-2k-window rate. ---
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunk_append.redb");
        let db = Db::open_with(&path, fts_spec()).unwrap();
        let scope = Scope::Id(ScopeId::from_u128(1));
        let total = 10_000usize;
        let mut r = Rng(0xC0FFEE);
        let mut batch: Vec<Op> = Vec::with_capacity(200);
        let start = std::time::Instant::now();
        let mut last_window_start = std::time::Instant::now();
        let mut done = 0usize;
        for i in 0..total {
            let content = fts_sentence(&mut r);
            let mut props = Props::new();
            props.insert("content".into(), PropValue::Str(content));
            batch.push(Op::CreateNode {
                id: NodeId::new(),
                scope,
                label: "Memory".into(),
                props,
            });
            if batch.len() == 200 || i + 1 == total {
                let n = batch.len();
                db.submit(std::mem::take(&mut batch)).unwrap();
                done += n;
                if done == total - 2_000 {
                    last_window_start = std::time::Instant::now();
                }
            }
        }
        let total_elapsed = start.elapsed();
        let last_window_elapsed = last_window_start.elapsed();
        println!(
            "APPEND total_docs={total} total_elapsed_s={:.2} overall_per_doc_ms={:.4} last_2k_per_doc_ms={:.4}",
            total_elapsed.as_secs_f64(),
            total_elapsed.as_secs_f64() * 1000.0 / total as f64,
            last_window_elapsed.as_secs_f64() * 1000.0 / 2_000.0
        );

        // Search latency at this chunk target (brief Step 2: "indexing cost
        // + search latency"). "agent" is one of the 16-word vocabulary terms
        // and near-universal across this corpus (worst-case: every chunk of
        // its posting list must be decoded to score it), on the SAME warm,
        // already-open `db` the append phase just built — 200 repeated
        // queries, min/median/p95/max by nearest-rank.
        let scopes = ScopeSet::of(&[ScopeId::from_u128(1)]);
        let sanity = db.search_text(&scopes, "agent", 10).unwrap();
        assert_eq!(
            sanity.len(),
            10,
            "chunk-target search-latency probe must hit k=10 in a 10k-doc near-universal-term corpus"
        );
        let mut times: Vec<std::time::Duration> = Vec::with_capacity(200);
        for _ in 0..200 {
            let start = std::time::Instant::now();
            db.search_text(&scopes, "agent", 10).unwrap();
            times.push(start.elapsed());
        }
        times.sort();
        let p95_idx = (times.len() * 95 / 100).min(times.len() - 1);
        println!(
            "SEARCH_LATENCY min_us={:.1} median_us={:.1} p95_us={:.1} max_us={:.1}",
            times[0].as_secs_f64() * 1e6,
            times[times.len() / 2].as_secs_f64() * 1e6,
            times[p95_idx].as_secs_f64() * 1e6,
            times[times.len() - 1].as_secs_f64() * 1e6
        );
    }

    // --- Edit-heavy phase: scaled-down version of `fts_edit_heavy_report`. ---
    {
        const BASE_DOCS: usize = 4_000;
        const MARKER_DOCS: usize = 200;
        const EDIT_BATCH: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunk_edit.redb");
        let db = Db::open_with(&path, fts_spec()).unwrap();
        let scope_id = ScopeId::from_u128(1);
        let scope = Scope::Id(scope_id);

        let mut r = Rng(0xBADC0DE);
        let mut ids = Vec::with_capacity(BASE_DOCS);
        let mut contents = Vec::with_capacity(BASE_DOCS);
        for _ in 0..BASE_DOCS {
            ids.push(NodeId::new());
        }
        let mut batch: Vec<Op> = Vec::with_capacity(200);
        for (i, &id) in ids.iter().enumerate() {
            let mut content = fts_sentence(&mut r);
            if i >= BASE_DOCS - MARKER_DOCS {
                content.push_str(" zzmarker");
            }
            contents.push(content.clone());
            let mut props = Props::new();
            props.insert("content".into(), PropValue::Str(content));
            batch.push(Op::CreateNode {
                id,
                scope,
                label: "Memory".into(),
                props,
            });
            if batch.len() == 200 || i + 1 == BASE_DOCS {
                db.submit(std::mem::take(&mut batch)).unwrap();
            }
        }

        let edit_candidates = BASE_DOCS - MARKER_DOCS;
        let checkpoints: Vec<usize> = [500usize, 1_500, 3_000]
            .into_iter()
            .filter(|&c| c <= edit_candidates)
            .collect();
        let total_edits = *checkpoints.last().unwrap();
        let mut edits_done = 0usize;
        let mut window_start = 0usize;
        let mut window_time = std::time::Instant::now();
        let mut next_checkpoint = 0usize;
        let mut idx = 0usize;
        while edits_done < total_edits {
            let batch_end = (idx + EDIT_BATCH).min(total_edits);
            let mut ops = Vec::with_capacity(batch_end - idx);
            for doc_i in idx..batch_end {
                let mut props: BTreeMap<String, Option<PropValue>> = BTreeMap::new();
                let new_content = format!("{} zzmarker", contents[doc_i]);
                props.insert("content".to_string(), Some(PropValue::Str(new_content)));
                ops.push(Op::SetNodeProps {
                    id: ids[doc_i],
                    props,
                });
            }
            let n = batch_end - idx;
            db.submit(ops).unwrap();
            edits_done += n;
            idx = batch_end;
            if next_checkpoint < checkpoints.len() && edits_done >= checkpoints[next_checkpoint] {
                let window_n = edits_done - window_start;
                let window_elapsed = window_time.elapsed();
                println!(
                    "EDIT_HEAVY edits={} window_n={window_n} window_elapsed_ms={:.2} per_edit_us={:.1}",
                    checkpoints[next_checkpoint],
                    window_elapsed.as_secs_f64() * 1000.0,
                    window_elapsed.as_secs_f64() * 1e6 / window_n as f64
                );
                next_checkpoint += 1;
                window_start = edits_done;
                window_time = std::time::Instant::now();
            }
        }
    }
}
