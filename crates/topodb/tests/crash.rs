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
    for round in 0..25 {
        let mut child = Command::new(&exe)
            .args(["crash_writer_child", "--exact", "--nocapture"])
            .env("TOPODB_CRASH_WRITER", &db_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Let it write for a pseudo-random slice (deterministic seed per
        // round — no wall-clock randomness needed).
        std::thread::sleep(Duration::from_millis(37 + (round * 61) % 211));
        child.kill().unwrap(); // SIGKILL on unix
        child.wait().unwrap();

        // (a) reopen succeeds…
        let db = topodb::Db::open(&db_path).unwrap();
        // (b) …and the state equals a replay of the surviving op log.
        verify_replay_equivalence(&db);
        drop(db);
    }
}
