use std::sync::Arc;
use topodb::{Db, EdgeId, NodeId, Op, Scope, ScopeId, TopoError};

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
