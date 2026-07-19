//! Kill-during-commit crash recovery: a child process (this same test
//! binary, re-executed with TOPODB_CRASH_WRITER set) writes batches in
//! a tight loop; the parent SIGKILLs it at a deterministic delay, reopens
//! the db, and verifies (a) open succeeds, (b) the state tables equal a
//! replay of the surviving op log — the engine's own determinism
//! machinery (`Db::rebuild_state_from_ops` plus the raw `debug_dump_*`
//! comparisons from `tests/determinism.rs`) is the verifier. Unix-only:
//! SIGKILL semantics.
#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::Duration;

// Child mode: loop writing 3-op batches (memory + entity + edge) until
// killed. Runs when the env var names the db path.
#[test]
fn crash_writer_child() {
    let Some(db_path) = std::env::var_os("TOPODB_CRASH_WRITER") else {
        return; // not in child mode: this "test" is a no-op
    };
    let db = topodb::Db::open(std::path::Path::new(&db_path)).unwrap();
    let scope = topodb::Scope::Shared;
    loop {
        let m = topodb::NodeId::new();
        let e = topodb::NodeId::new();
        let ops = vec![
            topodb::Op::CreateNode {
                id: m,
                scope,
                label: "Memory".into(),
                props: [(
                    "content".to_string(),
                    topodb::PropValue::Str("crash corpus".into()),
                )]
                .into_iter()
                .collect(),
            },
            topodb::Op::CreateNode {
                id: e,
                scope,
                label: "Entity".into(),
                props: [("name".to_string(), topodb::PropValue::Str(format!("E{m}")))]
                    .into_iter()
                    .collect(),
            },
            topodb::Op::CreateEdge {
                id: topodb::EdgeId::new(),
                scope,
                ty: "about".into(),
                from: m,
                to: e,
                props: Default::default(),
                valid_from: None,
            },
        ];
        let _ = db.submit(ops); // keep looping even on transient errors
    }
}

/// Reused from `tests/determinism.rs`'s
/// `state_from_replay_equals_state_from_execution`: that test captures
/// `debug_dump_nodes`/`debug_dump_edges`/`debug_dump_adjacency` (plus the
/// recall-layer raw dumps) *before* calling `rebuild_state_from_ops`, then
/// re-captures the same dumps *after*, and asserts entry-for-entry equality
/// — proving the disk-resident state tables are exactly what a full replay
/// of the op log produces. This lifts that exact idiom: whatever state a
/// reopened (possibly crash-truncated) db holds is "the state after
/// replaying the surviving op log" by construction (redb only durably
/// commits a transaction that appended a complete, valid entry to the op
/// log — see `storage.rs`), so comparing dumps before vs. after
/// `rebuild_state_from_ops` on the *same freshly reopened db* is precisely
/// the "state equals a replay of the surviving op log" check the brief
/// calls for, without needing a second from-scratch db to replay into.
fn verify_replay_equivalence(db: &topodb::Db) {
    let nodes_before = db.debug_dump_nodes();
    let edges_before = db.debug_dump_edges();
    let adjacency_before = db.debug_dump_adjacency().unwrap();
    let postings_before = db.debug_dump_postings().unwrap();
    let vectors_before = db.debug_dump_vectors().unwrap();
    let embedding_ref_before = db.debug_dump_embedding_ref().unwrap();
    let vector_dims_before = db.debug_dump_vector_dims().unwrap();

    db.rebuild_state_from_ops().unwrap();

    assert_eq!(
        nodes_before,
        db.debug_dump_nodes(),
        "NODES table must equal a replay of the surviving op log"
    );
    assert_eq!(
        edges_before,
        db.debug_dump_edges(),
        "EDGES table must equal a replay of the surviving op log"
    );
    assert_eq!(
        adjacency_before,
        db.debug_dump_adjacency().unwrap(),
        "OUT_ADJ/IN_ADJ must equal a replay of the surviving op log"
    );
    assert_eq!(
        postings_before,
        db.debug_dump_postings().unwrap(),
        "POSTINGS must equal a replay of the surviving op log"
    );
    assert_eq!(
        vectors_before,
        db.debug_dump_vectors().unwrap(),
        "VECTORS must equal a replay of the surviving op log"
    );
    assert_eq!(
        embedding_ref_before,
        db.debug_dump_embedding_ref().unwrap(),
        "EMBEDDING_REF must equal a replay of the surviving op log"
    );
    assert_eq!(
        vector_dims_before,
        db.debug_dump_vector_dims().unwrap(),
        "VECTOR_DIMS must equal a replay of the surviving op log"
    );
}

#[test]
fn killed_mid_write_recovers_and_replays_identically() {
    let exe = std::env::current_exe().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("crash.redb");
    // Cumulative node count across rounds: guards against a harness that
    // passes vacuously because a round's kill landed before the child ever
    // got a batch committed (verify_replay_equivalence alone can't tell
    // "wrote nothing" apart from "wrote and survived correctly" — both
    // dump-compare clean). From round 1 onward the count must strictly
    // increase; round 0 is allowed to start from zero in case spawn latency
    // ate the whole delay before the child even opened the db.
    let mut prev_count = 0usize;
    for round in 0..25 {
        let mut child = Command::new(&exe)
            .args(["crash_writer_child", "--exact", "--nocapture"])
            .env("TOPODB_CRASH_WRITER", &db_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Let it write for a pseudo-random slice (deterministic seed per
        // round — no wall-clock randomness needed). Floor is well above
        // this harness's measured process-spawn + first-`Db::open` +
        // first-commit latency (~1.0-1.1s under the sandboxed environment
        // this was authored in — process exec and initial redb table
        // creation are the dominant cost, not the write loop itself), so a
        // round's kill lands after the child has had a real chance to
        // commit at least one batch, not just after it opened the db.
        std::thread::sleep(Duration::from_millis(1300 + (round * 97) % 900));
        child.kill().unwrap(); // SIGKILL on unix
        child.wait().unwrap();

        // (a) reopen succeeds…
        let db = topodb::Db::open(&db_path).unwrap();
        // (b) …and the state equals a replay of the surviving op log.
        verify_replay_equivalence(&db);
        let count = db.debug_dump_nodes().len();
        eprintln!("round {round}: cumulative node count = {count}");
        if round >= 1 {
            assert!(
                count > prev_count,
                "round {round}: node count did not grow ({prev_count} -> {count}) — \
                 the child may have been killed before writing anything, which would \
                 make this round's equivalence check pass vacuously"
            );
        }
        prev_count = count;
        drop(db);
    }
    // Across the whole run, the writer must have landed a substantial
    // number of batches — proof this harness actually exercised the
    // kill-during-commit window many times over, not just once or twice.
    assert!(
        prev_count > 50,
        "final cumulative node count {prev_count} is too small to show the child \
         wrote meaningfully across the 25 rounds"
    );
}
