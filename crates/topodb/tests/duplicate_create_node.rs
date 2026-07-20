//! Final-review Finding 1/3 regression net: `CreateNode` ids are mint-once.
//! A `CreateNode` whose id already resolves to a node — in storage before
//! this submit, in an earlier batch of the SAME submit, or (via the group
//! pre-validation overlay) in an earlier batch of the same optimistic
//! commit group — must be rejected at live submit, never silently accepted
//! as an upsert. See `validate.rs::prevalidate_create_node_ids` and its
//! wiring in `db.rs`'s `apply_one_job`/`apply_group`.
//!
//! Replay tolerance (a historic op log predating this rejection may still
//! contain a duplicate-id create) and the `LABEL_INDEX` read-side defense
//! for it are covered separately, in `storage.rs`'s
//! `label_reads_skip_a_stale_index_row_whose_record_no_longer_matches` unit
//! test (the disproportionate full-replay-corpus variant was not shipped —
//! see the final-fix report).

use std::sync::Arc;
use topodb::*;

struct Fx {
    db: Db,
    _dir: tempfile::TempDir,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    Fx { db, _dir: dir }
}

fn create_node(id: NodeId, scope: Scope, label: &str) -> Op {
    Op::CreateNode {
        id,
        scope,
        label: label.into(),
        props: Default::default(),
    }
}

/// Two `CreateNode`s for the same id in ONE batch (submitted together) must
/// be rejected, and nothing from the batch commits — not even the OTHER
/// node the same batch tried to create.
#[test]
fn duplicate_id_within_the_same_batch_is_rejected() {
    let f = fx();
    let scope = Scope::Shared;
    let id = NodeId::new();
    let other = NodeId::new();

    let err =
        f.db.submit(vec![
            create_node(other, scope, "Entity"),
            create_node(id, scope, "Memory"),
            create_node(id, scope, "Memory"),
        ])
        .unwrap_err();
    assert!(
        matches!(err, TopoError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );

    let scopes = ScopeSet::default().with_shared();
    assert!(
        f.db.node(&scopes, id).is_none(),
        "the duplicated id must not exist"
    );
    assert!(
        f.db.node(&scopes, other).is_none(),
        "a rejected batch must leave storage untouched, including its OTHER ops"
    );
}

/// A `CreateNode` for an id that already exists from an EARLIER, already-
/// committed batch (the classic silent-upsert shape this finding closes)
/// must be rejected, and the original node must survive unchanged — not
/// overwritten by the rejected batch's label/props.
#[test]
fn duplicate_id_against_an_earlier_committed_batch_is_rejected() {
    let f = fx();
    let scope = Scope::Shared;
    let id = NodeId::new();

    f.db.submit(vec![create_node(id, scope, "Memory")]).unwrap();

    let err =
        f.db.submit(vec![create_node(id, scope, "Entity")])
            .unwrap_err();
    assert!(
        matches!(err, TopoError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );

    let scopes = ScopeSet::default().with_shared();
    let rec =
        f.db.node(&scopes, id)
            .expect("the original node must still exist");
    assert_eq!(
        rec.label, "Memory",
        "the original node's label must survive unchanged — no silent upsert"
    );
}

/// Same shape as the previous two tests, but racing the duplicate through
/// TWO CONCURRENT THREADS rather than sequential submits — the only way to
/// reach `db.rs::apply_group`'s optimistic group-commit path, where a
/// later-drained batch's pre-validation sees an earlier-in-the-SAME-group
/// batch's `CreateNode` via the `overlay` HashMap (not yet a durable
/// storage row) rather than via `Storage::load_nodes`. This is the exact
/// scenario Finding 3 required the old silent-upsert path to exist for:
/// two same-group batches racing to create the same id. Whether a given
/// round's pair actually lands in one shared group or two separate ones is
/// scheduler-dependent and not observable from here, so this doesn't assert
/// which path ran — it asserts the outcome that must hold either way:
/// exactly one of the two racing `CreateNode`s per id ever commits, and the
/// loser is cleanly `Rejected`, never a silent overwrite. Many rounds with
/// a `Barrier` synchronizing the two threads' `submit` calls make it likely
/// at least some rounds exercise the same-group overlay path over the
/// course of a full test run.
#[test]
fn duplicate_id_raced_by_two_threads_commits_exactly_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    let f = fx();
    let db = Arc::new(f.db);
    let scope = Scope::Shared;
    const ROUNDS: usize = 64;

    let mut a_ok = 0usize;
    let mut b_ok = 0usize;
    let rejected = AtomicUsize::new(0);

    for _ in 0..ROUNDS {
        let id = NodeId::new();
        let barrier = Arc::new(Barrier::new(2));

        let a_db = db.clone();
        let a_barrier = barrier.clone();
        let a = std::thread::spawn(move || {
            a_barrier.wait();
            a_db.submit(vec![create_node(id, scope, "WinnerA")])
        });

        let b_db = db.clone();
        let b_barrier = barrier.clone();
        let b = std::thread::spawn(move || {
            b_barrier.wait();
            b_db.submit(vec![create_node(id, scope, "WinnerB")])
        });

        let a_res = a.join().unwrap();
        let b_res = b.join().unwrap();

        let outcomes = [&a_res, &b_res];
        let successes = outcomes.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            successes, 1,
            "exactly one of the two racing same-id CreateNodes must commit \
             per round — got {a_res:?} / {b_res:?}"
        );
        for r in outcomes {
            match r {
                Ok(_) => {}
                Err(TopoError::Rejected(_)) => {
                    rejected.fetch_add(1, Ordering::Relaxed);
                }
                Err(other) => panic!("expected the loser to be Rejected, got {other:?}"),
            }
        }
        if a_res.is_ok() {
            a_ok += 1;
        } else {
            b_ok += 1;
        }
    }

    assert_eq!(
        a_ok + b_ok,
        ROUNDS,
        "every round must have exactly one winner"
    );
    assert_eq!(rejected.load(Ordering::Relaxed), ROUNDS);

    // No stale/duplicate index artifact from a would-be upsert: each
    // label's LABEL_INDEX hit count must equal exactly that label's win
    // count, and the two labels' counts must sum to ROUNDS.
    let scopes = ScopeSet::default().with_shared();
    let a_hits = db.nodes_by_label(&scopes, "WinnerA");
    let b_hits = db.nodes_by_label(&scopes, "WinnerB");
    assert_eq!(a_hits.len(), a_ok);
    assert_eq!(b_hits.len(), b_ok);
}
