//! Cold-open and on-disk attribution probe.
//!
//! Answers two questions the head-to-head numbers raise but cannot decompose:
//!
//!   1. Where does TopoDB's cold-open time go? Measured as raw
//!      `redb::Database::builder().create()` (the storage-layer floor, doing
//!      nothing TopoDB-specific) versus a full `TopoDbDriver::open`. The
//!      difference is everything TopoDB does on top: the table-creation write
//!      transaction and its commit, `Dicts::load`, `ScopeRegistry::load`, and
//!      `ensure_index_spec`'s second write transaction.
//!
//!   2. Where do the on-disk bytes go? Attributed per redb table via
//!      `TableStats`, so "the op log is why we are bigger" becomes a measured
//!      claim instead of an assumed one.
//!
//! No engine changes: both answers come from the public API plus redb's own
//! introspection. Node count is overridable via `BENCH_NODES`.
//!
//! Caveat on "cold": the file was just written, so it is in the OS page cache.
//! That matches the condition the existing point-query numbers were taken
//! under, so the two are comparable, but neither is a true cold-cache open.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use head_to_head::corpus::Corpus;
use head_to_head::engine::Engine;
use head_to_head::topodb_driver::TopoDbDriver;
use redb::{Database, ReadableTableMetadata, TableHandle};

const SEED: u64 = 20260719;
/// Alternating repeats of each open variant, to keep ordering bias and a
/// single unlucky sample from deciding the answer.
const OPEN_REPEATS: usize = 5;

fn median(mut xs: Vec<Duration>) -> Duration {
    xs.sort();
    xs[xs.len() / 2]
}

fn scratch_dir() -> PathBuf {
    let base = std::env::var("PROBE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("topodb-open-probe"));
    if base.exists() {
        std::fs::remove_dir_all(&base).expect("clear scratch dir");
    }
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

/// Raw redb open — the floor. Uses `create` (not `open`) because that is what
/// `Storage::open_with_options` itself calls, so this is the same operation
/// minus everything TopoDB layers on top.
fn time_raw_redb(path: &Path) -> Duration {
    let t = Instant::now();
    let db = Database::builder().create(path).expect("raw redb create");
    let d = t.elapsed();
    drop(db);
    d
}

fn time_full_open(path: &Path) -> Duration {
    let t = Instant::now();
    let db = TopoDbDriver::open(path).expect("topodb open");
    let d = t.elapsed();
    drop(db);
    d
}

fn attribute_disk(path: &Path) {
    let db = Database::builder().create(path).expect("open for stats");
    let r = db.begin_read().expect("begin read");

    let mut rows: Vec<(String, u64, u64, u64, u64)> = Vec::new();
    let handles: Vec<_> = r.list_tables().expect("list tables").collect();
    for h in handles {
        let name = h.name().to_string();
        let t = r.open_untyped_table(h).expect("open untyped table");
        let s = t.stats().expect("table stats");
        rows.push((
            name,
            t.len().expect("table len"),
            s.stored_bytes(),
            s.metadata_bytes(),
            s.fragmented_bytes(),
        ));
    }

    // Largest first: the point of this table is to see what dominates.
    rows.sort_by_key(|r| std::cmp::Reverse(r.2 + r.3 + r.4));

    let file_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let total: u64 = rows.iter().map(|r| r.2 + r.3 + r.4).sum();

    // Whole-database accounting, so the gap between "sum of user tables" and
    // "file bytes" is attributed rather than left as a mystery. `stats()`
    // lives on `WriteTransaction`, not `ReadTransaction`, so this takes a
    // write txn and aborts it -- the file is not modified.
    let wtx = db.begin_write().expect("begin write for stats");
    let ds = wtx.stats().expect("database stats");
    let allocated = ds.allocated_pages() * ds.page_size() as u64;
    let (ds_stored, ds_meta, ds_frag) = (
        ds.stored_bytes(),
        ds.metadata_bytes(),
        ds.fragmented_bytes(),
    );
    let (ds_alloc_pages, ds_page_size) = (ds.allocated_pages(), ds.page_size());
    drop(ds);
    wtx.abort().expect("abort stats txn");

    println!();
    println!("--- on-disk attribution ---");
    println!("file bytes:           {file_bytes}");
    println!(
        "allocated pages:      {} x {}B = {allocated} ({:.1}% of file)",
        ds_alloc_pages,
        ds_page_size,
        allocated as f64 / file_bytes.max(1) as f64 * 100.0
    );
    println!(
        "db stored/meta/frag:  {} / {} / {}",
        ds_stored,
        ds_meta,
        ds_frag
    );
    println!(
        "unallocated in file:  {} ({:.1}% of file)",
        file_bytes.saturating_sub(allocated),
        file_bytes.saturating_sub(allocated) as f64 / file_bytes.max(1) as f64 * 100.0
    );
    println!("sum of table bytes: {total}");
    println!(
        "{:<16} {:>10} {:>14} {:>14} {:>14} {:>8}",
        "table", "entries", "stored", "metadata", "fragmented", "% total"
    );
    for (name, len, stored, meta, frag) in &rows {
        let sub = stored + meta + frag;
        let pct = if total > 0 {
            sub as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        println!("{name:<16} {len:>10} {stored:>14} {meta:>14} {frag:>14} {pct:>7.1}%");
    }
}

fn main() {
    let nodes: usize = std::env::var("BENCH_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);

    let corpus = Corpus::generate(SEED, nodes);
    let ratio = corpus.translation_ratio();
    println!(
        "=== nodes={} facts={} (props={} edges={}) ===",
        nodes, ratio.facts, ratio.props, ratio.edges
    );

    let dir = scratch_dir();
    let path = dir.join("probe.redb");

    let t = Instant::now();
    {
        let mut db = TopoDbDriver::open(&path).expect("open for load");
        db.insert_corpus(&corpus).expect("insert corpus");
    }
    println!("load (open+insert_corpus): {:?}", t.elapsed());

    // Alternate the two variants so drift during the run hits both equally.
    let mut raw = Vec::new();
    let mut full = Vec::new();
    for _ in 0..OPEN_REPEATS {
        raw.push(time_raw_redb(&path));
        full.push(time_full_open(&path));
    }

    let raw_med = median(raw.clone());
    let full_med = median(full.clone());

    println!();
    println!("--- cold open attribution (medians of {OPEN_REPEATS}) ---");
    println!("raw redb create:      {raw_med:?}");
    println!("full TopoDbDriver:    {full_med:?}");
    println!(
        "topodb overhead:      {:?}  ({:.1}x raw)",
        full_med.saturating_sub(raw_med),
        full_med.as_secs_f64() / raw_med.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    println!("raw samples:  {raw:?}");
    println!("full samples: {full:?}");

    // `Storage::open_with_options` runs a write transaction (open/create every
    // table, then commit) and `ensure_index_spec` runs a second one, on EVERY
    // open including the steady-state no-migration path. An empty
    // begin_write+commit on the same file isolates what one such round trip
    // costs, with no engine changes.
    let mut empty_txn = Vec::new();
    for _ in 0..OPEN_REPEATS {
        let db = Database::builder().create(&path).expect("open");
        let t = Instant::now();
        let w = db.begin_write().expect("begin write");
        w.commit().expect("commit");
        empty_txn.push(t.elapsed());
    }
    let empty_med = median(empty_txn.clone());
    println!();
    println!("--- empty write txn (begin_write + commit), medians of {OPEN_REPEATS} ---");
    println!("one round trip:       {empty_med:?}");
    println!("two (what open does): {:?}", empty_med * 2);
    println!("measured overhead:    {:?}", full_med.saturating_sub(raw_med));
    println!("samples: {empty_txn:?}");

    attribute_disk(&path);

    // Hypothesis under test: the file is sized by peak copy-on-write usage
    // during bulk load and never reclaimed, so most of the gap between
    // "sum of table bytes" and "file bytes" is recoverable, and a smaller
    // file also opens faster. `compact()` is the minimal way to find out --
    // it changes no TopoDB code and no data, only the file's packing.
    let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let t = Instant::now();
    let compacted = {
        let mut db = Database::builder().create(&path).expect("open for compact");
        db.compact().expect("compact")
    };
    let compact_time = t.elapsed();
    let after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    println!();
    println!("--- compaction ---");
    println!("compact() returned: {compacted}, took {compact_time:?}");
    println!(
        "file bytes: {before} -> {after}  ({:.2}x smaller, {} reclaimed)",
        before as f64 / after.max(1) as f64,
        before.saturating_sub(after)
    );

    let mut raw2 = Vec::new();
    let mut full2 = Vec::new();
    for _ in 0..OPEN_REPEATS {
        raw2.push(time_raw_redb(&path));
        full2.push(time_full_open(&path));
    }
    let raw2_med = median(raw2.clone());
    let full2_med = median(full2.clone());
    println!();
    println!("--- cold open AFTER compaction (medians of {OPEN_REPEATS}) ---");
    println!("raw redb create:      {raw_med:?} -> {raw2_med:?}");
    println!("full TopoDbDriver:    {full_med:?} -> {full2_med:?}");
    println!(
        "topodb overhead:      {:?} -> {:?}",
        full_med.saturating_sub(raw_med),
        full2_med.saturating_sub(raw2_med)
    );
    println!("raw samples:  {raw2:?}");
    println!("full samples: {full2:?}");

    attribute_disk(&path);
}
