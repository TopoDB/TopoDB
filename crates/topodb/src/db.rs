use crate::error::TopoError;
use crate::graph::Snapshot;
use crate::ids::{EdgeId, NodeId};
use crate::op::Op;
use crate::storage::{AppliedBatch, Storage};
use arc_swap::ArcSwap;
use crossbeam_channel::{bounded, Sender};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

type Job = (Vec<Op>, Option<i64>, Sender<Result<AppliedBatch, TopoError>>);

/// A handle to an open database. Cloning shares the same underlying storage
/// and applier thread — `Db` is `Send + Sync + Clone`. All writes funnel
/// through a single applier thread (via `submit`/`submit_at`), so batches
/// serialize deterministically even under concurrent callers.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    // Read directly by `rebuild_state_from_ops`/`debug_dump_*`, and kept
    // alive here so the underlying `redb::Database`'s file handle stays open
    // for the lifetime of the `Db`, and for the (future) query layer that
    // will read through it directly.
    storage: Arc<Storage>,
    // In-memory adjacency snapshot. The applier thread is the *only* writer
    // (see `open`'s loop below); readers `load_full()` and never block on
    // it, and never on each other or on writers. Held behind its own `Arc`
    // (rather than the thread capturing an `Arc<Inner>`) so the applier
    // thread never holds a strong reference back to `Inner` itself — that
    // would create a cycle where `Inner`'s `Drop` (which must run to close
    // the channel so the thread can exit) never fires because the thread's
    // own clone keeps the refcount above zero.
    snap: Arc<ArcSwap<Snapshot>>,
    // `Sender` half of the job channel. Wrapped in `Option` so `Drop` can
    // `take()` it and actually drop it *before* joining the applier thread —
    // otherwise the applier's `rx.recv()` loop would never see the channel
    // close and `join()` would hang forever.
    tx: Mutex<Option<Sender<Job>>>,
    applier: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Db {
    /// Opens (creating if necessary) the database at `path` and starts its
    /// single applier thread. `submit`/`submit_at` route through this thread;
    /// it is the only place wall-clock time is read (`submit` uses
    /// `SystemTime::now`; `submit_at` is the deterministic test/backdate
    /// seam).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TopoError> {
        let storage = Arc::new(Storage::create(path)?);
        let initial_snapshot = Snapshot::from_storage(&storage)?;
        let snap = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));
        let (tx, rx) = bounded::<Job>(256);

        // The thread captures its own clones of `storage`/`snap` — never a
        // clone of `Inner` itself (see the comment on `Inner::snap` for why).
        let storage_for_applier = storage.clone();
        let snap_for_applier = snap.clone();
        let applier = std::thread::spawn(move || {
            while let Ok((ops, at, reply)) = rx.recv() {
                let now = at.unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("system clock before UNIX epoch")
                        .as_millis() as i64
                });
                match storage_for_applier.apply_batch(ops, now) {
                    Ok(batch) => {
                        // Fold the resolved ops into a new snapshot and
                        // store it *before* replying, so the submitter is
                        // guaranteed to observe its own write via
                        // `Db::snapshot`/the traversal helpers.
                        let cur = snap_for_applier.load_full();
                        let next = cur.apply(&batch.resolved, &|id| {
                            storage_for_applier.load_edge(id).ok().flatten()
                        });
                        snap_for_applier.store(Arc::new(next));
                        // If the caller already dropped its reply receiver,
                        // there's nothing to do with the result — move on.
                        let _ = reply.send(Ok(batch));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
        });

        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                snap,
                tx: Mutex::new(Some(tx)),
                applier: Mutex::new(Some(applier)),
            }),
        })
    }

    /// Returns the current in-memory adjacency snapshot. Cheap: an `Arc`
    /// clone via `ArcSwap::load_full` — never blocks on the applier thread
    /// or on other readers.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.snap.load_full()
    }

    /// Submits a batch of ops for application, blocking until the applier
    /// thread has processed it. Safe to call from any thread; batches from
    /// concurrent callers serialize through the single applier. Uses the
    /// wall clock (`SystemTime::now`) to resolve any unset timestamps.
    pub fn submit(&self, ops: Vec<Op>) -> Result<AppliedBatch, TopoError> {
        self.submit_inner(ops, None)
    }

    /// Like `submit`, but resolves unset timestamps to `now_ms` instead of
    /// the wall clock. Intended for tests and backdating.
    pub fn submit_at(&self, ops: Vec<Op>, now_ms: i64) -> Result<AppliedBatch, TopoError> {
        self.submit_inner(ops, Some(now_ms))
    }

    fn submit_inner(&self, ops: Vec<Op>, at: Option<i64>) -> Result<AppliedBatch, TopoError> {
        let (reply_tx, reply_rx) = bounded(1);
        let tx_guard = self.inner.tx.lock().unwrap();
        let tx = tx_guard.as_ref().ok_or(TopoError::Closed)?;
        tx.send((ops, at, reply_tx)).map_err(|_| TopoError::Closed)?;
        drop(tx_guard);
        reply_rx.recv().map_err(|_| TopoError::Closed)?
    }

    /// Test/inspection helper: every edge `(from, to)` currently in the
    /// adjacency snapshot, open or closed. `#[doc(hidden)]` — callers should
    /// prefer the query layer once it exists. Full `EdgeRecord`s (props
    /// included), read from the snapshot's `edges` map — the source of
    /// truth, not reconstructed from the lean `AdjEntry`s in `out`/`inn`.
    #[doc(hidden)]
    pub fn all_edges_between(&self, from: NodeId, to: NodeId) -> Vec<crate::state::EdgeRecord> {
        let snap = self.snapshot();
        snap.out
            .get(&from)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .filter(|e| e.other == to)
            .filter_map(|e| snap.edges.get(&e.edge).cloned())
            .collect()
    }

    /// Test/inspection helper: the ids of currently-open edges `(from, to)`
    /// (i.e. `valid_to.is_none()`). `#[doc(hidden)]` — see
    /// `all_edges_between`.
    #[doc(hidden)]
    pub fn open_edges_between(&self, from: NodeId, to: NodeId) -> Vec<EdgeId> {
        let snap = self.snapshot();
        snap.out
            .get(&from)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .filter(|e| e.other == to && e.valid_to.is_none())
            .map(|e| e.edge)
            .collect()
    }

    /// Rebuilds NODES/EDGES from the OPS log (see
    /// `Storage::rebuild_state_from_ops`) and swaps in a fresh
    /// `Snapshot::from_storage` so readers observe the rebuilt state — the
    /// existing snapshot is derived incrementally and would otherwise go
    /// stale relative to storage the moment the tables are drained.
    pub fn rebuild_state_from_ops(&self) -> Result<(), TopoError> {
        self.inner.storage.rebuild_state_from_ops()?;
        let fresh = Snapshot::from_storage(&self.inner.storage)?;
        self.inner.snap.store(Arc::new(fresh));
        Ok(())
    }

    /// Test/inspection helper: every node currently in storage, sorted by
    /// id for deterministic comparison. `#[doc(hidden)]` — see
    /// `all_edges_between`.
    #[doc(hidden)]
    pub fn debug_dump_nodes(&self) -> Vec<crate::state::NodeRecord> {
        let mut out = self.inner.storage.all_nodes().expect("debug dump: storage read failed");
        out.sort_by_key(|n| n.id);
        out
    }

    /// Test/inspection helper: every edge currently in storage, sorted by
    /// id for deterministic comparison. `#[doc(hidden)]` — see
    /// `all_edges_between`.
    #[doc(hidden)]
    pub fn debug_dump_edges(&self) -> Vec<crate::state::EdgeRecord> {
        let mut out = self.inner.storage.all_edges().expect("debug dump: storage read failed");
        out.sort_by_key(|e| e.id);
        out
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Drop the sender first so the applier's `rx.recv()` loop observes a
        // closed channel and exits, then join it. Without this, `tx` would
        // stay alive as a field on `Inner` until after we tried to join,
        // and the applier would block forever.
        self.tx.lock().unwrap().take();
        if let Some(h) = self.applier.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}
