use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use topodb::{
    Db, EdgeId, IndexSpec, NodeId, Op, PropIndex, PropValue, Props, Scope, ScopeId, ScopeSet,
    TopoError,
};

/// `cargo test` runs the tests in this file in parallel by default (no
/// `--test-threads=1` in the standard gate invocation for this binary). The
/// 16-writer stress test below and the timing-sensitive
/// `reads_complete_while_a_large_batch_commits` test can't share a process
/// without skewing each other: 16 extra OS threads hammering the CPU during
/// the timing test's writer batch inflates its wall time well past the
/// calibrated thresholds (observed directly — a batch that takes ~300-450ms
/// isolated took 823ms with the 16-writer test co-running). Grabbing this
/// lock at the top of every `#[test]` in the file forces them to run one at
/// a time, independent of the harness's test-level parallelism, without
/// needing `--test-threads=1` on the invocation.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Atlas conformance requirement: FactKey-style upsert under 16 concurrent
/// writers must serialize correctly — exactly one open edge at the end.
///
/// NOTE on the test design (approved amendment to the original brief): the
/// brief's verbatim test can fail on legitimate races — two writers may read
/// the same open set, one batch wins and the other is legitimately
/// `Rejected` (closing an edge that's already closed), or all 16 writers may
/// read before any writes land (zero closes, violating the brief's
/// `open.len() < all.len()` assert). Neither is a bug in `Db`; both are just
/// the read-then-write race the test is supposed to exercise. So: each
/// writer retries on `Rejected` until its own batch lands, guaranteeing
/// exactly 16 successful creates from the threads. Supersession itself is
/// then verified deterministically by one final supersede performed by the
/// main thread after all workers have joined.
#[test]
fn sixteen_writers_supersede_leaves_exactly_one_open_edge() {
    let _serialize = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let scope = Scope::Id(ScopeId::new());
    let (subject, value) = (NodeId::new(), NodeId::new());
    db.submit(vec![
        Op::CreateNode {
            id: subject,
            scope,
            label: "FactKey".into(),
            props: Default::default(),
        },
        Op::CreateNode {
            id: value,
            scope,
            label: "Entity".into(),
            props: Default::default(),
        },
    ])
    .unwrap();

    let db = Arc::new(db);
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                const MAX_ATTEMPTS: usize = 64;
                for _attempt in 0..MAX_ATTEMPTS {
                    // Supersede: close whatever is open, open a fresh edge —
                    // one batch. Re-read + rebuild fresh on every attempt so
                    // a retry after a lost race sees the current open set
                    // and uses a brand new EdgeId.
                    let open = db.open_edges_between(subject, value);
                    let mut ops: Vec<Op> =
                        open.into_iter().map(|e| Op::CloseEdge { id: e, valid_to: None }).collect();
                    ops.push(Op::CreateEdge {
                        id: EdgeId::new(),
                        scope,
                        ty: "HAS_VALUE".into(),
                        from: subject,
                        to: value,
                        props: Default::default(),
                        valid_from: None,
                    });
                    match db.submit(ops) {
                        Ok(_) => return,
                        Err(TopoError::Rejected(_)) => continue,
                        Err(e) => panic!("unexpected error from submit: {e}"),
                    }
                }
                panic!(
                    "writer thread exceeded {MAX_ATTEMPTS} retry attempts without a successful submit"
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // Exactly 16 successful creates from the threads (each thread succeeds
    // exactly once, by construction of the retry loop above).
    let after_threads = db.all_edges_between(subject, value);
    assert_eq!(after_threads.len(), 16);

    // Deterministic final supersede: one batch, closing every currently-open
    // edge and creating exactly one new one.
    let open = db.open_edges_between(subject, value);
    let mut final_ops: Vec<Op> = open
        .into_iter()
        .map(|e| Op::CloseEdge {
            id: e,
            valid_to: None,
        })
        .collect();
    final_ops.push(Op::CreateEdge {
        id: EdgeId::new(),
        scope,
        ty: "HAS_VALUE".into(),
        from: subject,
        to: value,
        props: Default::default(),
        valid_from: None,
    });
    db.submit(final_ops).unwrap();

    let all = db.all_edges_between(subject, value);
    assert_eq!(all.len(), 17);

    let open = db.open_edges_between(subject, value);
    assert_eq!(
        open.len(),
        1,
        "exactly one edge must remain open after the final supersede"
    );

    let closed_count = all.iter().filter(|e| e.valid_to.is_some()).count();
    assert_eq!(closed_count, 16, "every non-final edge must be closed");
}

/// F9b/F11b: once the write-guards (`dicts`/`scope_registry`) are held only
/// across interning — not across `tx.commit()`'s fsync — a reader running
/// concurrently with a large in-flight batch must never be serialized
/// behind that batch's commit.
///
/// Calibration (stated per the task brief): the writer's batch duration is
/// measured for real, since it varies with machine/CI load, and every
/// observed read duration must be under `max(batch_duration / 4, 250ms)` —
/// the LOOSER of a relative bound and an absolute one. A slow CI box that
/// makes the whole 4,000-op batch take, say, 2s shouldn't flake this test
/// just because a flat 250ms would be tight there; but any read that's
/// actually blocked behind the guard takes close to the FULL batch
/// duration, which blows past `batch_duration / 4` regardless of how slow
/// the machine is. Pre-F9b the guard spans `tx.commit()`, so a read landing
/// mid-batch blocks until the writer's commit+fsync finishes; post-F9b the
/// guard is dropped before `tx.commit()`, so reads race past the
/// commit/fsync in low single-digit milliseconds.
#[test]
fn reads_complete_while_a_large_batch_commits() {
    let _serialize = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Note".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let scope_id = ScopeId::new();
    let scope = Scope::Id(scope_id);
    let scopes = ScopeSet::of(&[scope_id]);

    // Small readable corpus, seeded and committed BEFORE the big batch so
    // reads during the batch have something to find (nodes_by_label + a
    // text search that actually matches).
    let mut seed_props = Props::new();
    seed_props.insert(
        "content".into(),
        PropValue::Str("rust embedded database engine".into()),
    );
    db.submit(vec![Op::CreateNode {
        id: NodeId::new(),
        scope,
        label: "Note".into(),
        props: seed_props,
    }])
    .unwrap();

    let db = Arc::new(db);
    let writer_done = Arc::new(AtomicBool::new(false));

    // Writer thread: ONE batch of 4,000 CreateNode ops — big enough that
    // apply + commit takes hundreds of ms, giving the reader loop plenty of
    // chances to land mid-batch.
    let writer_db = db.clone();
    let writer_done_flag = writer_done.clone();
    let writer = std::thread::spawn(move || {
        let ops: Vec<Op> = (0..4_000)
            .map(|_| Op::CreateNode {
                id: NodeId::new(),
                scope,
                label: "Bulk".into(),
                props: Props::new(),
            })
            .collect();
        let start = Instant::now();
        writer_db.submit(ops).unwrap();
        let elapsed = start.elapsed();
        // Release: timings collected under `Acquire` after this flips must
        // see a fully-committed batch, but the flag is only a loop
        // terminator here (not a correctness fence for the reads
        // themselves), so a simple store/load pair is enough.
        writer_done_flag.store(true, Ordering::Release);
        elapsed
    });

    // Reader thread: loop until the writer finishes, timing EACH call.
    let reader_db = db.clone();
    let reader_scopes = scopes.clone();
    let reader_done_flag = writer_done.clone();
    let reader = std::thread::spawn(move || {
        let mut timings = Vec::new();
        while !reader_done_flag.load(Ordering::Acquire) {
            let t0 = Instant::now();
            let _ = reader_db.nodes_by_label(&reader_scopes, "Note");
            timings.push(t0.elapsed());

            let t1 = Instant::now();
            reader_db
                .search_text(&reader_scopes, "embedded rust", 5)
                .unwrap();
            timings.push(t1.elapsed());
        }
        timings
    });

    let batch_duration = writer.join().unwrap();
    let timings = reader.join().unwrap();

    assert!(
        !timings.is_empty(),
        "reader never got a chance to run concurrently with the writer \
         (batch finished before any read fired) — grow the batch or shrink \
         per-read overhead so the two threads actually overlap"
    );

    let threshold = std::cmp::max(batch_duration / 4, Duration::from_millis(250));
    for (i, d) in timings.iter().enumerate() {
        assert!(
            *d < threshold,
            "read #{i} took {d:?}, exceeding threshold {threshold:?} \
             (batch_duration={batch_duration:?}); reads must never block \
             behind an in-flight batch's commit/fsync"
        );
    }

    // No correctness regression: the batch itself succeeded and a final
    // read sees all 4,000 new nodes.
    let bulk = db.nodes_by_label(&scopes, "Bulk");
    assert_eq!(bulk.len(), 4_000);
}

/// F9c regression net: this test's shape (many independently-valid
/// single-op batches from many threads) is EXACTLY the workload the
/// applier's drain-and-group-commit optimization targets — the perf claim
/// itself is the bench's job (`cargo bench -p topodb --bench storage --
/// concurrent_submit_16`), not this test's. This test passed on the
/// per-batch commit path BEFORE Task 6 and must keep passing unchanged
/// after it: every submit still succeeds, and the change feed — fed once
/// per batch, in submission order, whether batches share a commit or not —
/// still delivers a gapless, strictly-increasing sequence with no
/// duplicates and no drops.
#[test]
fn concurrent_submitters_all_succeed_and_feed_stays_ordered() {
    let _serialize = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let db = Arc::new(db);

    const THREADS: usize = 16;
    const BATCHES_PER_THREAD: usize = 32;
    const TOTAL: usize = THREADS * BATCHES_PER_THREAD;

    // Subscribe BEFORE any concurrent submits so every batch's broadcast is
    // captured. Generous capacity (well above `TOTAL`) keeps the
    // best-effort broadcast (`db.rs`'s `broadcast_batch`: a full buffer
    // silently drops the event) from ever firing at this small a volume —
    // this test is checking ordering/gaplessness, not the drop-on-full
    // behavior (that's `change_feed.rs`'s job).
    let feed = db.subscribe(TOTAL * 4);

    let scope = Scope::Id(ScopeId::new());
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                for _ in 0..BATCHES_PER_THREAD {
                    db.submit(vec![Op::CreateNode {
                        id: NodeId::new(),
                        scope,
                        label: "Concurrent".into(),
                        props: Default::default(),
                    }])
                    .expect(
                        "every batch in this test is independently valid — \
                         optimistic group commit (F9c) must never poison a \
                         sibling batch drained into the same group",
                    );
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // One op per batch, one seq per op: `current_seq` must equal the total
    // submit count exactly (no batch was silently dropped or duplicated).
    let current_seq = db.current_seq().unwrap();
    assert_eq!(
        current_seq as usize, TOTAL,
        "current_seq must equal the total submit count"
    );

    // Drain the feed: seqs must be strictly increasing with NO gaps, from 1
    // through current_seq, with exactly `TOTAL` events — regardless of
    // which batches ended up sharing a group commit, the applier still
    // broadcasts once per batch, per-batch, in submission order (see
    // `apply_group`'s doc comment).
    let mut seqs = Vec::with_capacity(TOTAL);
    for _ in 0..TOTAL {
        let ev = feed
            .recv_timeout(Duration::from_secs(5))
            .expect("feed should have delivered every one of this test's batches within 5s");
        seqs.push(ev.seq);
    }
    assert!(
        feed.try_recv().is_err(),
        "no extra events beyond the expected total"
    );
    for w in seqs.windows(2) {
        assert_eq!(
            w[1],
            w[0] + 1,
            "feed seqs must be strictly increasing with no gaps: {seqs:?}"
        );
    }
    assert_eq!(seqs.first().copied(), Some(1));
    assert_eq!(seqs.last().copied(), Some(current_seq));
}

/// F9c regression net: a batch that fails mid-apply (the Task 4 shape —
/// `CreateNode` followed by `RemoveNode` on an id that was never created,
/// so the second op always rejects) must never poison a sibling batch that
/// happens to land in the same optimistic group. This test passed on the
/// per-batch commit path BEFORE Task 6 (there each batch already got its
/// own transaction) and must keep passing unchanged after it — the failure
/// mode Task 6 introduces risk for is a GROUP-committed sibling being
/// dragged down by another batch's failure, which `apply_batches`'s
/// whole-group-abort + `apply_group`'s individual-replay fallback (see
/// `db.rs`) exists specifically to prevent.
#[test]
fn failing_batch_in_a_group_poisons_nothing() {
    let _serialize = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let db = Arc::new(db);
    let scope_id = ScopeId::new();
    let scope = Scope::Id(scope_id);

    const ROUNDS: usize = 64;

    // Thread A: a valid single-op batch every round — always independently
    // committable.
    let a_db = db.clone();
    let a_handle = std::thread::spawn(move || {
        for _ in 0..ROUNDS {
            a_db.submit(vec![Op::CreateNode {
                id: NodeId::new(),
                scope,
                label: "Good".into(),
                props: Default::default(),
            }])
            .expect(
                "thread A's batches are always independently valid — a \
                     sibling group batch's failure must never poison them",
            );
        }
    });

    // Thread B: the Task 4 failure shape every round — `RemoveNode` always
    // targets a freshly generated id that was never created, so the batch
    // fails mid-apply every time, on both the optimistic group-commit path
    // and the individual-replay fallback.
    let b_db = db.clone();
    let b_handle = std::thread::spawn(move || {
        let mut rejected = 0usize;
        for _ in 0..ROUNDS {
            match b_db.submit(vec![
                Op::CreateNode {
                    id: NodeId::new(),
                    scope,
                    label: "Bad".into(),
                    props: Default::default(),
                },
                Op::RemoveNode { id: NodeId::new() },
            ]) {
                Err(TopoError::Rejected(_)) => rejected += 1,
                other => panic!("expected every thread-B batch to be Rejected, got {other:?}"),
            }
        }
        rejected
    });

    a_handle.join().unwrap();
    let b_rejected = b_handle.join().unwrap();
    assert_eq!(b_rejected, ROUNDS, "every thread-B batch must be rejected");

    // Final state contains exactly A's nodes (count check): every "Good"
    // node landed, and no "Bad" node survived — proving a rejected batch's
    // `CreateNode` never partially committed even though its `RemoveNode`
    // failed afterward.
    let scopes = ScopeSet::of(&[scope_id]);
    let good = db.nodes_by_label(&scopes, "Good");
    assert_eq!(good.len(), ROUNDS, "every thread-A node must be present");
    let bad = db.nodes_by_label(&scopes, "Bad");
    assert!(
        bad.is_empty(),
        "no Bad-labeled node may survive a rejected batch"
    );

    // Replay equivalence (Task 3's `verify_replay_equivalence` idiom,
    // inlined: dump state, rebuild from the op log on the same db, dump
    // again, assert equality) — whatever is on disk must be exactly what
    // replaying the surviving op log produces.
    let nodes_before = db.debug_dump_nodes();
    let edges_before = db.debug_dump_edges();
    db.rebuild_state_from_ops().unwrap();
    assert_eq!(
        nodes_before,
        db.debug_dump_nodes(),
        "NODES must equal a replay of the surviving op log"
    );
    assert_eq!(
        edges_before,
        db.debug_dump_edges(),
        "EDGES must equal a replay of the surviving op log"
    );
}
