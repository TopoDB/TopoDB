//! Point-query verification binary (Task 7).
//!
//! Builds both engines at a corpus size derived from
//! `Corpus::translation_ratio()` (not guessed), then measures `point_lookup`
//! cold (fresh open + one lookup, open and lookup timed separately) and warm
//! (N lookups of *varied* ids, median and mean reported). Exists to check,
//! on this machine, whether minigraf's published "point query @ 1M facts:
//! 4.3-4.5s" reproduces -- see
//! `docs/superpowers/notes/2026-07-19-point-query-verification.md` for the
//! answer.
//!
//! Node count is overridable via `BENCH_NODES` (an exact node count, not a
//! fact count) so the same binary can be re-run at other scales.

use std::path::Path;
use std::time::{Duration, Instant};

use head_to_head::corpus::Corpus;
use head_to_head::engine::Engine;
use head_to_head::minigraf_driver::MinigrafDriver;
use head_to_head::topodb_driver::TopoDbDriver;

const SEED: u64 = 20260719;
const WARM_N: usize = 200;

/// Search for the node count whose `translation_ratio().facts` is closest to
/// `target`. The nodes/edges relationship is not perfectly linear (edges are
/// drawn from a growing backward-reference window, and per-node edge counts
/// are randomised 1..=3), but it converges in a handful of rescale
/// iterations for corpora of this size. Returns (node_count, facts) of the
/// best candidate seen.
fn nodes_for_target_facts(target: usize, seed: u64) -> (usize, usize) {
    let mut candidate = (target as f64 / 7.0).round().max(2.0) as usize;
    let mut best: Option<(usize, usize)> = None; // (node_count, facts)

    for _ in 0..10 {
        let facts = Corpus::generate(seed, candidate).translation_ratio().facts;
        let better = match best {
            None => true,
            Some((_, best_facts)) => {
                (facts as i64 - target as i64).abs() < (best_facts as i64 - target as i64).abs()
            }
        };
        if better {
            best = Some((candidate, facts));
        }
        if facts == target {
            break;
        }
        let scale = target as f64 / facts.max(1) as f64;
        let next = ((candidate as f64) * scale).round().max(2.0) as usize;
        if next == candidate {
            break;
        }
        candidate = next;
    }
    best.expect("at least one candidate evaluated")
}

struct LoadResult {
    load_time: Duration,
}

struct ColdResult {
    open_time: Duration,
    first_lookup_time: Duration,
    first_lookup_hit: bool,
}

struct WarmResult {
    n: usize,
    median: Duration,
    mean: Duration,
    min: Duration,
    max: Duration,
}

fn median_mean(mut durs: Vec<Duration>) -> (Duration, Duration, Duration, Duration) {
    durs.sort();
    let n = durs.len();
    let median = if n % 2 == 0 {
        (durs[n / 2 - 1] + durs[n / 2]) / 2
    } else {
        durs[n / 2]
    };
    let total: Duration = durs.iter().sum();
    let mean = total / n as u32;
    (median, mean, durs[0], durs[n - 1])
}

/// Build, then measure cold open+lookup, then warm lookups, for one engine.
/// `build` performs open+insert_corpus and returns the loaded handle plus the
/// load time; `reopen` performs a fresh `Engine::open` against the same
/// on-disk path (approximating "cold" within the same process -- true
/// process-level isolation is not attempted, so this is stated explicitly in
/// the report as a limitation).
fn measure<E: Engine>(
    label: &str,
    path: &Path,
    corpus: &Corpus,
    node_count: usize,
) -> (LoadResult, ColdResult, WarmResult, u64) {
    // --- Load ---
    let t0 = Instant::now();
    let mut db = E::open(path).expect("open for load");
    db.insert_corpus(corpus).expect("insert_corpus");
    let load_time = t0.elapsed();
    drop(db);

    // --- Cold: fresh open, then exactly one lookup ---
    let t0 = Instant::now();
    let cold_db = E::open(path).expect("cold open");
    let open_time = t0.elapsed();

    let cold_id = 0usize;
    let t0 = Instant::now();
    let cold_payload = cold_db.point_lookup(cold_id).expect("cold point_lookup");
    let first_lookup_time = t0.elapsed();
    let first_lookup_hit = cold_payload.is_some();

    // --- Warm: same handle, WARM_N lookups of varied ids via a fixed stride ---
    // Stride chosen so ids are spread across the whole id space rather than
    // clustered, and is not a divisor of node_count so ids don't repeat
    // early.
    let stride = (node_count / WARM_N).max(1) | 1; // force odd, avoid trivial small-stride cycles
    let mut durs = Vec::with_capacity(WARM_N);
    let mut hits = 0usize;
    for i in 0..WARM_N {
        let id = (i * stride) % node_count;
        let t0 = Instant::now();
        let payload = cold_db.point_lookup(id).expect("warm point_lookup");
        durs.push(t0.elapsed());
        if payload.is_some() {
            hits += 1;
        }
    }
    assert!(
        hits > WARM_N / 2,
        "{label}: warm lookups mostly missed ({hits}/{WARM_N}) -- ids likely wrong"
    );
    let (median, mean, min, max) = median_mean(durs);

    let on_disk = cold_db.on_disk_bytes().expect("on_disk_bytes");

    (
        LoadResult { load_time },
        ColdResult {
            open_time,
            first_lookup_time,
            first_lookup_hit,
        },
        WarmResult {
            n: WARM_N,
            median,
            mean,
            min,
            max,
        },
        on_disk,
    )
}

fn fmt_dur(d: Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{:.3} s", d.as_secs_f64())
    } else if d.as_micros() >= 1000 {
        format!("{:.3} ms", d.as_secs_f64() * 1e3)
    } else {
        format!("{} µs", d.as_micros())
    }
}

struct ScaleReport {
    node_count: usize,
    facts: usize,
    topo: (LoadResult, ColdResult, WarmResult, u64),
    mini: (LoadResult, ColdResult, WarmResult, u64),
}

fn run_scale(node_count: usize, facts_hint: Option<usize>) -> ScaleReport {
    let corpus = Corpus::generate(SEED, node_count);
    let ratio = corpus.translation_ratio();
    eprintln!(
        "=== scale: {node_count} nodes -> {} facts ({} props + {} edges){} ===",
        ratio.facts,
        ratio.props,
        ratio.edges,
        facts_hint
            .map(|f| format!(" [target was {f}]"))
            .unwrap_or_default(),
    );

    let base = std::env::temp_dir().join(format!(
        "h2h-point-query-{}-{}",
        node_count,
        std::process::id()
    ));
    std::fs::create_dir_all(&base).expect("create temp base dir");

    let topo_path = base.join("topodb.redb");
    let mini_path = base.join("minigraf.graph");

    eprintln!("--- building topodb ---");
    let topo = measure::<TopoDbDriver>("topodb", &topo_path, &corpus, node_count);
    eprintln!("--- building minigraf ---");
    let mini = measure::<MinigrafDriver>("minigraf", &mini_path, &corpus, node_count);

    let _ = std::fs::remove_dir_all(&base);

    ScaleReport {
        node_count,
        facts: ratio.facts,
        topo,
        mini,
    }
}

fn print_scale(r: &ScaleReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n## Scale: {} nodes, {} facts\n\n",
        r.node_count, r.facts
    ));
    out.push_str("| Metric | TopoDB | Minigraf |\n");
    out.push_str("|---|---|---|\n");
    out.push_str(&format!(
        "| Load (open+insert_corpus) | {} | {} |\n",
        fmt_dur(r.topo.0.load_time),
        fmt_dur(r.mini.0.load_time)
    ));
    out.push_str(&format!(
        "| Cold open | {} | {} |\n",
        fmt_dur(r.topo.1.open_time),
        fmt_dur(r.mini.1.open_time)
    ));
    out.push_str(&format!(
        "| Cold first lookup (hit={}/{}) | {} | {} |\n",
        r.topo.1.first_lookup_hit,
        r.mini.1.first_lookup_hit,
        fmt_dur(r.topo.1.first_lookup_time),
        fmt_dur(r.mini.1.first_lookup_time)
    ));
    out.push_str(&format!(
        "| Warm median (N={}) | {} | {} |\n",
        r.topo.2.n,
        fmt_dur(r.topo.2.median),
        fmt_dur(r.mini.2.median)
    ));
    out.push_str(&format!(
        "| Warm mean (N={}) | {} | {} |\n",
        r.topo.2.n,
        fmt_dur(r.topo.2.mean),
        fmt_dur(r.mini.2.mean)
    ));
    out.push_str(&format!(
        "| Warm min / max | {} / {} | {} / {} |\n",
        fmt_dur(r.topo.2.min),
        fmt_dur(r.topo.2.max),
        fmt_dur(r.mini.2.min),
        fmt_dur(r.mini.2.max)
    ));
    out.push_str(&format!(
        "| On-disk bytes | {} | {} |\n",
        r.topo.3, r.mini.3
    ));
    out
}

fn main() {
    let mut scales: Vec<ScaleReport> = Vec::new();

    if let Ok(override_nodes) = std::env::var("BENCH_NODES") {
        let n: usize = override_nodes.parse().expect("BENCH_NODES must be a usize");
        scales.push(run_scale(n, None));
    } else {
        let target = 1_000_000usize;
        let (node_count, facts) = nodes_for_target_facts(target, SEED);
        eprintln!(
            "derived node_count={node_count} for target facts={target} (actual facts={facts})"
        );
        let one_m = run_scale(node_count, Some(target));

        // If minigraf's warm median point lookup is >= 100ms at 1M facts,
        // also measure at ~100k facts to see how it scales.
        let mini_slow = one_m.mini.2.median >= Duration::from_millis(100);
        if mini_slow {
            eprintln!("minigraf warm median >= 100ms at 1M facts; also measuring at ~100k facts");
        }
        scales.push(one_m);

        if mini_slow {
            let target_small = 100_000usize;
            let (n_small, facts_small) = nodes_for_target_facts(target_small, SEED);
            eprintln!(
                "derived node_count={n_small} for target facts={target_small} (actual facts={facts_small})"
            );
            scales.push(run_scale(n_small, Some(target_small)));
        }
    }

    let mut report = String::new();
    report.push_str("# Point-query verification\n\n");
    report.push_str(&format!(
        "Generated by `benchmarks/head-to-head/src/bin/point_query.rs`, seed {SEED}.\n"
    ));
    for s in &scales {
        report.push_str(&print_scale(s));
        println!("{}", print_scale(s));
    }

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("benchmarks/head-to-head has two ancestor components");
    let out_path_buf =
        repo_root.join("docs/superpowers/notes/2026-07-19-point-query-verification.md");
    let out_path = out_path_buf.as_path();
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(out_path, report).expect("write results file");
    eprintln!("wrote {}", out_path.display());
}
