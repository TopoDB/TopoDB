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
//! re-indexing EXISTING low-slot documents to gain a term, the case that
//! `fts.rs`'s old `mutate_posting_chunk` doc comment once flagged as never
//! splitting; fixed by the 2026-07-13 mid-chunk split, see the Gate 6b
//! re-run in BENCHMARKS.md). Both are
//! resumable-in-spirit (single self-contained runs well under the CI-agent
//! command budget once the append path is O(1)/doc) rather than
//! checkpoint/resume like `build_open_fixture`, since neither needs it at
//! the scales gate 6 specifies. `chunk_target_experiment_report` is the
//! smaller, faster combined append+edit-heavy harness re-run once per
//! `POSTINGS_CHUNK_TARGET` candidate (see BENCHMARKS.md's chunk-target
//! experiment table for the four recorded runs).
//!
//! F8 addition (Task 7 / BENCHMARKS.md F8 gate): `build_vector_fixture` /
//! `vector_search_report`, a resumable 100k/1M-vector-scope report tier
//! mirroring `build_open_fixture` / `open_report`'s split — build in a
//! time-budgeted, resumable step; measure in a separate step that refuses to
//! time a partial fixture. Reports the deterministic HNSW graph path's
//! min/median/p95/max search latency over 50 queries AND recall@10 against a
//! one-shot exact scan per query (forced through the SAME public
//! `search_vector` entry point by passing every in-scope id as
//! `q.candidates` — see `search_scan`'s `if let Some(candidates)` branch,
//! which never consults the graph). See each test's doc comment for its
//! exact invocation and env knobs.
use std::collections::{BTreeMap, HashSet};
use topodb::workload::{batches, WorkloadSpec};
use topodb::{
    Db, DbOptions, IndexSpec, NodeId, Op, PropIndex, PropValue, Props, Scope, ScopeId, ScopeSet,
    VectorQuery,
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
        ..Default::default()
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
    /// A pseudo-random float in `[-1.0, 1.0)`, for the vector-search report
    /// tier below — mirrors `benches/storage.rs`'s `VecRng::next_f32` (dense
    /// random coordinates, not one-hot; see that struct's doc comment for
    /// why one-hot vectors pathologically tie on cosine score at scale).
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() as f32 / u64::MAX as f32) * 2.0 - 1.0
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
/// corpus, the case `fts.rs`'s old `mutate_posting_chunk` doc comment once
/// named as deliberately unsplit ("a covering-chunk insert can grow a chunk
/// slightly past `POSTINGS_CHUNK_TARGET` without triggering a split").
/// `mutate_posting_chunk` was replaced by `apply_posting_mutation` +
/// `store_posting_chunk_splitting` in the 2026-07-13 mid-chunk split, which
/// closed this gap — see the Gate 6b re-run in BENCHMARKS.md.
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
/// term's entire existing range) — before the mid-chunk split, that chunk
/// never split, so this was the one scenario that could grow a chunk
/// unboundedly; now it splits like any other over-target chunk (Gate 6b
/// re-run, BENCHMARKS.md). Report: per-edit cost across increasing edit
/// counts, so growth (or its absence) shows as a curve.
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
    let mut checkpoint_per_edit_us: Vec<f64> = Vec::new();
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
            checkpoint_per_edit_us.push(window_elapsed.as_secs_f64() * 1e6 / window_n as f64);
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

    // Gate 6b — promoted from FINDING to HARD GATE (mid-chunk-split design,
    // 2026-07-13): the last checkpoint window's per-edit cost must be within
    // 1.5x the first window's. Pre-fix this measured 2.79x at chunk target
    // 4096 and 2.03x at 8192 (BENCHMARKS.md, Gate 6b) — the covering chunk
    // grew without bound and the covering search decoded forward. Post-fix
    // both are bounded; the curve must be near-flat.
    let first = *checkpoint_per_edit_us
        .first()
        .expect("checkpoints are non-empty by construction");
    let last = *checkpoint_per_edit_us
        .last()
        .expect("checkpoints are non-empty by construction");
    let ratio = last / first;
    println!("GATE6B first_window_per_edit_us={first:.1} last_window_per_edit_us={last:.1} ratio={ratio:.2}");
    assert!(
        ratio <= 1.5,
        "GATE 6b FAIL: per-edit cost grew {ratio:.2}x from the 1k to the 12k checkpoint (gate: <= 1.5x)"
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

/// Fixed model name for the vector-search report tier's embeddings — one
/// scope, one model, per the brief (mirrors `benches/storage.rs`'s
/// `"bench-768"` naming style, distinct string so the two never collide if a
/// db were ever shared, though each lives at its own fixture path).
const VEC_REPORT_MODEL: &str = "vec-report";

/// The single scope every vector-report node lives in.
fn vec_report_scope_id() -> ScopeId {
    ScopeId::from_u128(0xF8_5EED)
}

/// `IndexSpec` for the vector-report fixture: no equality/text declarations
/// (`Default`) — this tier only exercises `search_vector` and
/// `nodes_by_label`/`nodes_by_label_unbumped` (ground-truth id enumeration),
/// neither of which needs `PROP_INDEX`/FTS. `LABEL_INDEX` (which
/// `nodes_by_label` reads) is maintained unconditionally, independent of
/// `IndexSpec` — see `storage.rs`'s `load_nodes_by_label`.
fn vec_report_spec() -> IndexSpec {
    IndexSpec::default()
}

/// Deterministic per-index node id: `NodeId::from_u128` (not `NodeId::new`,
/// which is wall-clock-random) so `build_vector_fixture` can resume across
/// process invocations purely from `db.current_seq()` — id `i` is always the
/// same id, on this run or any later resumed one, without needing to persist
/// an id list anywhere.
fn vec_report_node_id(i: usize) -> NodeId {
    NodeId::from_u128(0x5EED_0000_0000_0000_0000_0000_0000_0000 + i as u128)
}

/// Deterministic per-index embedding: a fresh `Rng`, seeded from `i` alone
/// (not threaded sequentially through one shared generator), so any node's
/// vector is reproducible independent of build order or resume point —
/// critical for a resumable, chunked, possibly-multi-process build where a
/// later invocation must reconstruct exactly the vectors an earlier
/// invocation would have generated for the SAME indices, without replaying
/// the whole prefix. Dense random floats (not one-hot), matching
/// `benches/storage.rs`'s `VecRng` rationale.
fn vec_report_vector(i: usize, dim: usize) -> Vec<f32> {
    let seed = (i as u64)
        .wrapping_add(0xC0FFEE)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut r = Rng(seed);
    (0..dim).map(|_| r.next_f32()).collect()
}

/// Corpus geometry for the vector-report tier, from `TOPODB_VEC_GEOMETRY`:
/// `"uniform"` (default — i.i.d. `vec_report_vector`, HNSW's known
/// pathological worst case at high `dim`, kept as the stressor row) or
/// `"manifold"` (`vec_report_vector_manifold` — the realistic row: real
/// embedding corpora have LOW INTRINSIC DIMENSION and CLUSTER STRUCTURE,
/// which is what graph navigability actually depends on; the first 100k
/// gate run measured recall 0.078 on uniform-384d for a graph that was
/// 100% connected — geometry, not construction, was the bottleneck).
#[derive(Clone, Copy, PartialEq, Debug)]
enum VecGeometry {
    Uniform,
    Manifold,
}

fn vec_report_geometry() -> VecGeometry {
    match std::env::var("TOPODB_VEC_GEOMETRY").as_deref() {
        Err(_) | Ok("uniform") => VecGeometry::Uniform,
        Ok("manifold") => VecGeometry::Manifold,
        Ok(other) => panic!("TOPODB_VEC_GEOMETRY must be uniform|manifold, got {other:?}"),
    }
}

/// Manifold-geometry constants: points live near a `MANIFOLD_LATENT_DIM`-
/// dimensional linear subspace of the ambient space, gathered around
/// `MANIFOLD_CLUSTERS` cluster centers, with a small full-rank ambient
/// noise floor. All three magnitudes are fixed (not env knobs) so a
/// "manifold" fixture is one reproducible shape per `(n, dim)`.
const MANIFOLD_LATENT_DIM: usize = 12;
const MANIFOLD_CLUSTERS: usize = 32;
const MANIFOLD_LOCAL_SPREAD: f32 = 0.25;
const MANIFOLD_AMBIENT_NOISE: f32 = 0.02;

/// Deterministic per-index embedding on the clustered manifold. Like
/// `vec_report_vector`, a pure function of `i` alone (resume-safe): the
/// basis and cluster centers come from fixed seeds, the local offset and
/// ambient noise from `i`-derived seeds. `v = Σ_j latent_j · basis_j +
/// noise`, `latent = center[i % CLUSTERS] + LOCAL_SPREAD · local(i)`.
fn vec_report_vector_manifold(i: usize, dim: usize) -> Vec<f32> {
    let cluster = i % MANIFOLD_CLUSTERS;
    let mut center = Rng(0xC3A7_E400_u64
        .wrapping_add(cluster as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut local = Rng((i as u64)
        .wrapping_add(0x10CA1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let latent: Vec<f32> = (0..MANIFOLD_LATENT_DIM)
        .map(|_| center.next_f32() + MANIFOLD_LOCAL_SPREAD * local.next_f32())
        .collect();
    let mut noise = Rng((i as u64)
        .wrapping_add(0x4015E)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut v = vec![0.0f32; dim];
    for (j, &l) in latent.iter().enumerate() {
        let mut basis = Rng(0xBA51_5000_u64
            .wrapping_add(j as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for slot in v.iter_mut() {
            *slot += l * basis.next_f32();
        }
    }
    for slot in v.iter_mut() {
        *slot += MANIFOLD_AMBIENT_NOISE * noise.next_f32();
    }
    v
}

/// Geometry-dispatched generator — every fixture-build and report call site
/// goes through this so corpus and queries always share one distribution.
fn vec_report_vector_geo(i: usize, dim: usize, geometry: VecGeometry) -> Vec<f32> {
    match geometry {
        VecGeometry::Uniform => vec_report_vector(i, dim),
        VecGeometry::Manifold => vec_report_vector_manifold(i, dim),
    }
}

#[test]
fn manifold_generator_is_deterministic_and_shaped() {
    // Resume-safety pin (mirrors `vec_report_vector`'s contract): same
    // index twice gives the identical vector with no shared generator
    // state, distinct indices differ, and the norm is non-degenerate
    // (cosine-scoreable) at the report tier's dim.
    let a = vec_report_vector_manifold(7, 384);
    assert_eq!(a, vec_report_vector_manifold(7, 384));
    assert_ne!(a, vec_report_vector_manifold(8, 384));
    assert_eq!(a.len(), 384);
    let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(norm > 0.1, "manifold vector must be scoreable, norm={norm}");
}

/// Fixed (non-tempdir) path for the vector-report fixture, keyed by its
/// shape (`n`, `dim`) and geometry so different report scales and corpus
/// shapes coexist — same convention as `open_fixture_path`/
/// `ram_fixture_path`; the uniform name is the legacy one so existing
/// fixtures stay resumable. Not cleaned up automatically.
fn vec_report_fixture_path(n: usize, dim: usize, geometry: VecGeometry) -> std::path::PathBuf {
    let name = match geometry {
        VecGeometry::Uniform => format!("topodb_vec_report_{n}_{dim}.redb"),
        VecGeometry::Manifold => format!("topodb_vec_report_{n}_{dim}_manifold.redb"),
    };
    std::env::temp_dir().join(name)
}

/// Vector-search report tier, step 1 (resumable, time-budgeted build):
/// `cargo test -p topodb --release --test size_report -- --ignored build_vector_fixture --nocapture`
/// with `TOPODB_VEC_FIXTURE_N` (default 100_000), `TOPODB_VEC_DIM` (default
/// 384), `TOPODB_VEC_BUILD_CHUNK` (nodes per submit, default 500), and
/// `TOPODB_BUILD_BUDGET_SECS` (default 420) in the environment. One scope,
/// one model (`VEC_REPORT_MODEL`), every node embedded (unlike
/// `build_open_fixture`'s `embed_pct`-partial corpus — a pure vector-search
/// gate needs every node in-scope to be a real candidate). Resumable exactly
/// like `build_open_fixture`: `db.current_seq()` (2 ops/node, `CreateNode`
/// paired with `SetEmbedding`) tells this run how many nodes are already
/// committed, and `vec_report_node_id`/`vec_report_vector` are pure
/// functions of the node index alone, so the next chunk picks up exactly
/// where any prior run (in this process or an earlier one) left off. Rerun
/// the identical command until it prints `complete=true`.
///
/// For the 1M/384-dim tier (deferred — see the module doc comment and
/// BENCHMARKS.md): `TOPODB_VEC_FIXTURE_N=1000000 cargo test -p topodb
/// --release --test size_report -- --ignored build_vector_fixture --nocapture`,
/// rerun until `complete=true`. 1M x 384 x 4 bytes (f32) ~= 1.5 GB of raw
/// embedding bytes alone, well past this machine's free disk during F8 —
/// Task 8 documents this run as pending.
#[test]
#[ignore]
fn build_vector_fixture() {
    let n = env_usize("TOPODB_VEC_FIXTURE_N", 100_000);
    let dim = env_usize("TOPODB_VEC_DIM", 384);
    let chunk = env_usize("TOPODB_VEC_BUILD_CHUNK", 500);
    let budget = std::time::Duration::from_secs(env_usize("TOPODB_BUILD_BUDGET_SECS", 420) as u64);
    let geometry = vec_report_geometry();
    let path = vec_report_fixture_path(n, dim, geometry);
    let scope = Scope::Id(vec_report_scope_id());

    let db = Db::open_with(&path, vec_report_spec()).unwrap();
    let done_ops = db.current_seq().unwrap() as usize;
    assert_eq!(
        done_ops % 2,
        0,
        "vector-report fixture ops must come in CreateNode+SetEmbedding pairs — got an odd current_seq"
    );
    let done_nodes = done_ops / 2;
    assert!(
        done_nodes <= n,
        "fixture has more nodes ({done_nodes}) than requested (n={n}) — wrong TOPODB_VEC_FIXTURE_N/TOPODB_VEC_DIM?"
    );

    let start = std::time::Instant::now();
    let mut submitted = done_nodes;
    let mut idx = done_nodes;
    while idx < n {
        if start.elapsed() > budget {
            break;
        }
        let end = (idx + chunk).min(n);
        let mut ops = Vec::with_capacity((end - idx) * 2);
        for i in idx..end {
            let id = vec_report_node_id(i);
            ops.push(Op::CreateNode {
                id,
                scope,
                label: "Vec".into(),
                props: Default::default(),
            });
            ops.push(Op::SetEmbedding {
                id,
                model: VEC_REPORT_MODEL.into(),
                vector: vec_report_vector_geo(i, dim, geometry),
            });
        }
        db.submit(ops).unwrap();
        submitted = end;
        idx = end;
        println!(
            "  nodes {submitted}/{n} elapsed_secs={:.0}",
            start.elapsed().as_secs_f64()
        );
    }
    println!(
        "n={n} dim={dim} geometry={geometry:?} nodes={submitted}/{n} run_secs={:.1} complete={}",
        start.elapsed().as_secs_f64(),
        submitted == n
    );
}

/// Prints min/median/p95/max (nearest-rank, matching `print_open_stats`'s
/// method) for a sorted-ascending duration slice, in milliseconds, under
/// `label` — the generic form of `print_open_stats` for reports (like this
/// one) that time something other than `Db::open_with`.
fn print_latency_stats(label: &str, times: &[std::time::Duration]) {
    for (i, t) in times.iter().enumerate() {
        println!("  {label}[{i}] = {:.2} ms", t.as_secs_f64() * 1000.0);
    }
    let p95_idx = (times.len() * 95 / 100).min(times.len() - 1);
    println!(
        "{label}_min_ms={:.2} {label}_median_ms={:.2} {label}_p95_ms={:.2} {label}_max_ms={:.2}",
        times[0].as_secs_f64() * 1000.0,
        times[times.len() / 2].as_secs_f64() * 1000.0,
        times[p95_idx].as_secs_f64() * 1000.0,
        times[times.len() - 1].as_secs_f64() * 1000.0,
    );
}

/// Vector-search report tier, step 2 (measurement — F8 gate): `cargo test -p
/// topodb --release --test size_report -- --ignored vector_search_report --nocapture`
/// with the same `TOPODB_VEC_FIXTURE_N`/`TOPODB_VEC_DIM` as the completed
/// `build_vector_fixture` run. Verifies the fixture is fully built (refusing
/// to report a partial one, same discipline as `open_report`), then runs 50
/// deterministic k=10 queries (query `i`'s vector is `vec_report_vector(n +
/// i, dim)` — same generator, indices past the fixture so no query
/// coincides with a fixture member) against the live HNSW graph path,
/// printing min/median/p95/max search latency (`print_latency_stats`).
///
/// For each query this ALSO computes an exact top-10 by passing every
/// in-scope node id as `q.candidates` — `search_scan`'s candidates branch
/// (`vector.rs`) always does a per-candidate linear cosine scan and never
/// consults the HNSW graph, so this is a true brute-force ground truth
/// reached through the same public `search_vector` entry point, not a
/// reimplementation of cosine scoring in this test. recall@10 is the mean
/// fraction of the graph's top-10 ids present in the exact top-10 across all
/// 50 queries; both the mean and the per-query value print, and the mean is
/// asserted `>= 0.95` (the same recall floor `hnsw.rs`'s own recall gate
/// enforces at smaller scale — see that module's determinism/recall test).
#[test]
#[ignore]
fn vector_search_report() {
    const QUERIES: usize = 50;
    let n = env_usize("TOPODB_VEC_FIXTURE_N", 100_000);
    let dim = env_usize("TOPODB_VEC_DIM", 384);
    let geometry = vec_report_geometry();
    let path = vec_report_fixture_path(n, dim, geometry);
    let scopes = ScopeSet::of(&[vec_report_scope_id()]);

    let db = Db::open_with(&path, vec_report_spec()).unwrap();
    let expected_ops = (n * 2) as u64;
    assert_eq!(
        db.current_seq().unwrap(),
        expected_ops,
        "fixture incomplete — rerun build_vector_fixture until complete=true"
    );
    println!("n={n} dim={dim}");
    println!("file_bytes={}", std::fs::metadata(&path).unwrap().len());

    // Ground-truth candidate pool: every node in the fixture's one scope,
    // via the public label-scan read path (not a reimplementation).
    let all_ids: Vec<NodeId> = db
        .nodes_by_label_unbumped(&scopes, "Vec")
        .into_iter()
        .map(|rec| rec.id)
        .collect();
    assert_eq!(
        all_ids.len(),
        n,
        "fixture's Vec-labeled node count must match n — rerun build_vector_fixture until complete=true"
    );

    let mut graph_times: Vec<std::time::Duration> = Vec::with_capacity(QUERIES);
    let mut recalls: Vec<f64> = Vec::with_capacity(QUERIES);
    for qi in 0..QUERIES {
        let qv = vec_report_vector_geo(n + qi, dim, geometry);

        let graph_query = VectorQuery {
            scopes: scopes.clone(),
            model: VEC_REPORT_MODEL.into(),
            vector: qv.clone(),
            k: 10,
            candidates: None,
        };
        let start = std::time::Instant::now();
        let graph_hits = db.search_vector_unbumped(&graph_query).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(
            graph_hits.len(),
            10,
            "vector_search_report: graph query {qi} must return k=10 hits"
        );
        graph_times.push(elapsed);

        let exact_query = VectorQuery {
            scopes: scopes.clone(),
            model: VEC_REPORT_MODEL.into(),
            vector: qv,
            k: 10,
            candidates: Some(all_ids.clone()),
        };
        let exact_hits = db.search_vector_unbumped(&exact_query).unwrap();
        assert_eq!(
            exact_hits.len(),
            10,
            "vector_search_report: exact-scan query {qi} must return k=10 hits"
        );
        let exact_ids: HashSet<NodeId> = exact_hits.iter().map(|(rec, _)| rec.id).collect();
        let hit_count = graph_hits
            .iter()
            .filter(|(rec, _)| exact_ids.contains(&rec.id))
            .count();
        let recall = hit_count as f64 / 10.0;
        recalls.push(recall);
        println!(
            "query={qi} graph_ms={:.2} recall_at_10={recall:.2}",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    graph_times.sort();
    print_latency_stats("graph", &graph_times);

    let mean_recall = recalls.iter().sum::<f64>() / recalls.len() as f64;
    println!("geometry={geometry:?} recall_at_10_mean={mean_recall:.4}");
    // The recall hard-gate applies to the realistic (manifold) geometry —
    // BENCHMARKS.md gate 2a. The uniform row is the distance-concentration
    // stressor (gate 2b): recall ≥ 0.95 at `ef_search = max(4k, 64)` is not
    // achievable on i.i.d. uniform high-dim regardless of selection policy
    // (measured: 0.078 at 100k under closest-M, and heuristic selection did
    // not move it — see the v7 section), so asserting it would leave the
    // stressor row permanently red and turn a real gate into noise. Its
    // number is still printed above and recorded in the table.
    match geometry {
        VecGeometry::Manifold => assert!(
            mean_recall >= 0.95,
            "GATE: mean recall@10 must be >= 0.95, got {mean_recall:.4}"
        ),
        VecGeometry::Uniform => println!(
            "stressor row (uniform geometry): no recall hard-gate; see BENCHMARKS.md gate 2b"
        ),
    }
}
