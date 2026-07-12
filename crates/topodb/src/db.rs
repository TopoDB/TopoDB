use crate::counters::AccessStats;
use crate::error::TopoError;
use crate::feed::ChangeEvent;
use crate::ids::{EdgeId, NodeId, ScopeSet};
use crate::index::IndexSpec;
use crate::op::Op;
use crate::storage::{AppliedBatch, Storage};
use crate::vector::VectorIndex;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Tuning knobs for [`Db::open_with_options`]. Additive: every field
/// defaults to `None`, under which redb's own default is used, so a fresh
/// `DbOptions::default()` behaves identically to `Db::open`/`Db::open_with`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DbOptions {
    /// Threaded straight to `redb::Builder::set_cache_size`. `None` leaves
    /// redb's own default (1 GiB, split 90/10 read/write) in place.
    pub cache_size_bytes: Option<usize>,
}

/// A unit of work for the single applier thread. Both variants carry a reply
/// channel so the submitting thread blocks until the applier has finished —
/// and, crucially, so the *applier* remains the sole writer of storage.
enum Job {
    Apply {
        ops: Vec<Op>,
        at: Option<i64>,
        reply: Sender<Result<AppliedBatch, TopoError>>,
    },
    Rebuild {
        reply: Sender<Result<(), TopoError>>,
    },
    /// Fire-and-forget batch of access-counter bumps folded into COUNTERS by
    /// the applier. No reply channel: bumps are auxiliary telemetry, so the
    /// applier logs nothing, broadcasts nothing to the change feed, and never
    /// acknowledges. Enqueued only by the bumper thread (see `open_with`).
    BumpCounters { bumps: Vec<(NodeId, u64, i64)> },
    /// Compacts the op log through `keep_from` on the applier thread (the sole
    /// redb writer). Broadcasts nothing — compaction touches no NODES/EDGES
    /// state and emits no change events — and replies the storage result so the
    /// caller blocks until the trim has committed.
    Compact {
        keep_from: u64,
        reply: Sender<Result<(), TopoError>>,
    },
}

/// A handle to an open database. Cloning shares the same underlying storage
/// and applier thread — `Db` is `Send + Sync + Clone`. All writes funnel
/// through a single applier thread (via `submit`/`submit_at`), so batches
/// serialize deterministically even under concurrent callers.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

// Manual (not derived) so this doesn't force `Debug` on every field of
// `Inner` (several of which — `Storage` among them — don't derive it and
// aren't otherwise worth adding it to). `Db` itself carries no useful
// state to print; this exists so `Result<Db, TopoError>` — e.g. in a test's
// `panic!("{other:?}")` fallback arm — is formattable.
impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

struct Inner {
    // Read directly by `rebuild_state_from_ops`/`debug_dump_*`/every scoped
    // read (`node`, `nodes_by_label`, `traverse`, ...), and kept alive here
    // so the underlying `redb::Database`'s file handle stays open for the
    // lifetime of the `Db`. The read model is disk-resident: there is no
    // separate in-memory snapshot to keep in step with it (see FORMAT.md /
    // the W5 plan task for the snapshot layer this replaced).
    storage: Arc<Storage>,
    // `Sender` half of the job channel. Wrapped in `Option` so `Drop` can
    // `take()` it and actually drop it *before* joining the applier thread —
    // otherwise the applier's `rx.recv()` loop would never see the channel
    // close and `join()` would hang forever.
    tx: Mutex<Option<Sender<Job>>>,
    applier: Mutex<Option<std::thread::JoinHandle<()>>>,
    // `Sender` half of the bump channel feeding the bumper thread. Reads
    // `try_send` `(NodeId, ts)` pairs here; the bumper accumulates and flushes
    // them as batched `Job::BumpCounters`. Wrapped in `Option` so `Drop` can
    // take+drop it *first* (before joining the bumper) — closing this channel
    // is what makes the bumper's `recv_timeout` loop see `Disconnected`, do its
    // final flush, and exit. See `Drop for Inner` for the full ordering.
    bump_tx: Mutex<Option<Sender<(NodeId, i64)>>>,
    bumper: Mutex<Option<std::thread::JoinHandle<()>>>,
    // Change-feed subscriber registry: the bounded `Sender` half of every
    // live `subscribe` channel. The applier clones this `Arc` at spawn and is
    // the *only* broadcaster; `subscribe` pushes a new sender under the mutex.
    // Both hold the lock only briefly (a push, or one non-blocking drain per
    // batch), and nothing else locks it — so it introduces no lock-ordering
    // hazard against the `tx`/`applier` mutexes. Held behind its own `Arc`
    // (not captured via `Inner`) for the same reason as `storage`: the
    // applier must never hold a strong ref back to `Inner`, or `Drop` would
    // deadlock.
    subs: Arc<Mutex<Vec<Sender<ChangeEvent>>>>,
    // Per-(model, scope) f32 embedding slabs. Held behind its own `Arc`
    // (never captured via `Inner`, same rationale as `storage`/`subs`): the
    // applier holds a clone and is the sole mutator of the outer slab map
    // (slab creation, and the wholesale swap on rebuild); searches take short
    // read locks. See `vector.rs` for the locking contract.
    vectors: Arc<VectorIndex>,
}

impl Db {
    /// Opens (creating if necessary) the database at `path` and starts its
    /// single applier thread. `submit`/`submit_at` route through this thread;
    /// it is the only place wall-clock time is read (`submit` uses
    /// `SystemTime::now`; `submit_at` is the deterministic test/backdate
    /// seam). Delegates to `open_with` with a default (empty) `IndexSpec`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TopoError> {
        Self::open_with(path, IndexSpec::default())
    }

    /// Opens `path` using the `IndexSpec` persisted in its META (written by
    /// `Storage::ensure_index_spec` on every prior open), so callers need not
    /// supply one. A fresh file, or one predating spec persistence (no
    /// `index_spec` key), opens with `IndexSpec::default()`.
    ///
    /// Idempotent: the persisted spec is passed straight back through
    /// `open_with`, so `ensure_index_spec` sees an unchanged text list and no
    /// FTS reindex is triggered — the equality index is declared exactly as
    /// the file was created. A transient extra (read-only) open of the file
    /// is used to peek the spec before the real `open_with`.
    pub fn open_stored(path: impl AsRef<Path>) -> Result<Self, TopoError> {
        let path = path.as_ref();
        let spec = Storage::read_persisted_index_spec(path)?.unwrap_or_default();
        Self::open_with(path, spec)
    }

    /// Like `open`, but with a declared `IndexSpec` governing which
    /// `(label, prop)` pairs get equality/text-indexed. `spec` is validated
    /// (rejecting duplicate declarations) before anything else happens — an
    /// invalid spec never touches storage. Delegates to `open_with_options`
    /// with `DbOptions::default()`.
    pub fn open_with(path: impl AsRef<Path>, spec: IndexSpec) -> Result<Self, TopoError> {
        Self::open_with_options(path, spec, DbOptions::default())
    }

    /// Like `open_with`, but also takes [`DbOptions`] governing storage
    /// tuning knobs (currently just `cache_size_bytes`, threaded to redb's
    /// `Builder::set_cache_size`).
    pub fn open_with_options(
        path: impl AsRef<Path>,
        spec: IndexSpec,
        options: DbOptions,
    ) -> Result<Self, TopoError> {
        spec.validate()?;
        let spec = Arc::new(spec);
        let storage = Arc::new(Storage::open_with_options(path, spec, options)?);
        // Build the vector index by scanning EMBEDDINGS (the one remaining
        // open-time scan — see `VectorIndex::from_storage`). The applier
        // captures a clone below.
        let vectors = Arc::new(VectorIndex::from_storage(&storage)?);
        let (tx, rx) = bounded::<Job>(256);

        // The thread captures its own clones of `storage`/`vectors`/`subs` —
        // never a clone of `Inner` itself (see the comment on `Inner::storage`
        // for why: a strong ref back to `Inner` would create a cycle where
        // `Inner`'s `Drop` never fires).
        let storage_for_applier = storage.clone();
        let subs: Arc<Mutex<Vec<Sender<ChangeEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let subs_for_applier = subs.clone();
        let vectors_for_applier = vectors.clone();
        let applier = std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                match job {
                    Job::Apply { ops, at, reply } => {
                        let now = at.unwrap_or_else(|| {
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .expect("system clock before UNIX epoch")
                                .as_millis() as i64
                        });
                        // Read pre-batch node state (scope, embedding) for
                        // every id this batch might reference — CreateEdge
                        // endpoints, SetEmbedding targets, RemoveNode
                        // targets — in ONE storage read, BEFORE `apply_batch`
                        // runs. This anchors three things: dim pre-validation
                        // (below), edge-scope pre-validation (below), and
                        // slab maintenance (after `apply_batch`, which still
                        // needs the OLD state — storage holds only the
                        // post-batch state once `apply_batch` has committed).
                        let pre = match storage_for_applier.load_nodes(&ids_needing_pre_state(&ops))
                        {
                            Ok(m) => m,
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                continue;
                            }
                        };
                        // Dim pre-validation runs BEFORE `apply_batch` so a
                        // violation leaves storage untouched — atomic with the
                        // rest of the batch.
                        if let Err(e) = vectors_for_applier.prevalidate_dims(&pre, &ops) {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        // Edge-scope pre-validation has the same contract: reject
                        // before `apply_batch` so storage is untouched. It must not
                        // live in `apply_op`, which is shared with op-log replay.
                        if let Err(e) = crate::validate::prevalidate_edge_scopes(&pre, &ops) {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        match storage_for_applier.apply_batch(ops, now) {
                            Ok(batch) => {
                                // Slab maintenance runs after `apply_batch`
                                // succeeds, using `pre` (read before
                                // `apply_batch`) for old embedding/scope
                                // state.
                                vectors_for_applier.maintain(&pre, &batch.resolved);
                                // Broadcast the committed ops to live
                                // subscribers *after* `apply_batch` has
                                // committed (so a subscriber that reacts by
                                // reading sees its own event's effect) and
                                // *before* replying.
                                // Best-effort, non-blocking: a full subscriber
                                // buffer drops the event (the subscriber
                                // detects the `seq` gap and recovers via
                                // `ops_since`); a disconnected receiver is
                                // pruned. The applier NEVER blocks on a slow
                                // subscriber. Only successful `Job::Apply`
                                // batches broadcast — rejects and rebuilds
                                // emit nothing.
                                // Wrap each op in an `Arc` ONCE per op for the
                                // whole batch (not once per op per
                                // subscriber) — every subscriber below then
                                // only pays for a cheap `Arc::clone`.
                                let ev_ops: Vec<Arc<Op>> = batch
                                    .resolved
                                    .iter()
                                    .map(|op| Arc::new(op.clone()))
                                    .collect();
                                let mut subs = subs_for_applier.lock().unwrap();
                                subs.retain(|s| {
                                    for (i, ev_op) in ev_ops.iter().enumerate() {
                                        let ev = ChangeEvent {
                                            seq: batch.first_seq + i as u64,
                                            op: ev_op.clone(),
                                        };
                                        match s.try_send(ev) {
                                            Ok(()) => {}
                                            Err(crossbeam_channel::TrySendError::Full(_)) => {}
                                            Err(crossbeam_channel::TrySendError::Disconnected(
                                                _,
                                            )) => return false,
                                        }
                                    }
                                    true
                                });
                                drop(subs);
                                // If the caller already dropped its reply
                                // receiver, there's nothing to do with the
                                // result — move on.
                                let _ = reply.send(Ok(batch));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                    Job::Rebuild { reply } => {
                        // Rebuild runs on the applier thread — the sole redb
                        // writer — so it serializes with in-flight batch
                        // application: `rebuild_state_from_ops` and
                        // `apply_batch` can never interleave.
                        let result = match storage_for_applier.rebuild_state_from_ops() {
                            // Rebuild the vector index from the now-rebuilt
                            // storage and swap the inner slab map in place
                            // (under the outer write lock) — the applier's
                            // `Arc<VectorIndex>` is shared with `Db`, so only
                            // its contents may be replaced, not the `Arc`.
                            Ok(()) => vectors_for_applier.rebuild_from(&storage_for_applier),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(result);
                    }
                    Job::BumpCounters { bumps } => {
                        // Auxiliary telemetry: fold into COUNTERS and move on.
                        // Deliberately NO op-log append and NO change-feed
                        // broadcast (the feed's broadcast lives only in the
                        // `Job::Apply` success arm above and stays there) — and
                        // no reply, since bumps are fire-and-forget. A failed
                        // write is swallowed: losing best-effort counters must
                        // never take down the applier.
                        let _ = storage_for_applier.merge_counter_bumps(&bumps);
                    }
                    Job::Compact { keep_from, reply } => {
                        // Runs on the applier (sole redb writer), so it
                        // serializes with batch application: no append can
                        // interleave between the delete and the `oldest_seq`
                        // stamp. Compaction touches only the OPS/META tables —
                        // never NODES/EDGES — so there is nothing to
                        // broadcast.
                        let _ = reply.send(storage_for_applier.compact_ops_through(keep_from));
                    }
                }
            }
        });

        // Bumper thread: owns batching of access-counter bumps so reads never
        // pay a per-hit write. It holds a *clone* of the applier `Sender` and
        // forwards accumulated bumps as `Job::BumpCounters`. Because of that
        // clone, `Drop for Inner` MUST join this thread *before* dropping the
        // applier `tx` — otherwise the applier channel never closes and the
        // applier join hangs (see `Drop for Inner`).
        let (bump_tx, bump_rx) = bounded::<(NodeId, i64)>(4096);
        let applier_tx_for_bumper = tx.clone();
        let bumper = std::thread::spawn(move || {
            let mut pending: std::collections::HashMap<NodeId, (u64, i64)> = Default::default();
            let flush = |pending: &mut std::collections::HashMap<NodeId, (u64, i64)>| {
                if pending.is_empty() {
                    return;
                }
                let bumps: Vec<(NodeId, u64, i64)> =
                    pending.drain().map(|(id, (n, ts))| (id, n, ts)).collect();
                // Applier gone (shutdown race) → drop silently; aux data.
                let _ = applier_tx_for_bumper.send(Job::BumpCounters { bumps });
            };
            loop {
                match bump_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok((id, ts)) => {
                        let e = pending.entry(id).or_insert((0, 0));
                        e.0 += 1;
                        e.1 = e.1.max(ts);
                        if pending.len() >= 256 {
                            flush(&mut pending);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => flush(&mut pending),
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        flush(&mut pending);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                tx: Mutex::new(Some(tx)),
                applier: Mutex::new(Some(applier)),
                subs,
                bump_tx: Mutex::new(Some(bump_tx)),
                bumper: Mutex::new(Some(bumper)),
                vectors,
            }),
        })
    }

    /// The shared vector index. Cheap `Arc` clone; used by `search_vector`
    /// (in `vector.rs`) to reach the slab map from an `impl Db` block in a
    /// sibling module that can't touch `self.inner` directly.
    #[must_use]
    pub(crate) fn vectors(&self) -> Arc<VectorIndex> {
        self.inner.vectors.clone()
    }

    /// The underlying storage. Used by `search_text` (in `fts.rs`) to open a
    /// read transaction over the POSTINGS/FTS_DOCS/META tables from an
    /// `impl Db` block in a sibling module that can't touch `self.inner`.
    #[must_use]
    pub(crate) fn storage(&self) -> &Storage {
        &self.inner.storage
    }

    /// The on-disk format version of the opened file (delegates to
    /// `Storage::format_version`). Added so `topodb-cli`'s `info` can report
    /// it without reaching into crate internals.
    pub fn format_version(&self) -> u32 {
        // `Storage::format_version` only fails on a missing/malformed META
        // row, which `open_with` guarantees exists (it writes it on first
        // create and validates it on every open) — unreachable for a `Db`
        // that has successfully opened.
        self.inner
            .storage
            .format_version()
            .expect("format_version: META row guaranteed by a successful open")
    }

    /// The `IndexSpec` this db is operating under — the one `open_stored`
    /// resolved (or the one passed to `open_with`). Added so `info` can
    /// report it. A clone of `Storage`'s own copy (the source of truth —
    /// there is no longer a separate snapshot-carried copy to read instead).
    #[must_use]
    pub fn index_spec(&self) -> IndexSpec {
        (*self.inner.storage.spec).clone()
    }

    /// Per-table logical byte counts; benchmark/inspection seam.
    #[doc(hidden)]
    pub fn storage_report(&self) -> Result<Vec<crate::storage::TableReport>, TopoError> {
        self.inner.storage.storage_report()
    }

    /// Records an access bump for each id in `ids`, timestamped with a single
    /// wall-clock read taken once per call. Fire-and-forget: each `(id, now)`
    /// is `try_send`'d to the bumper thread, and on a full or closed channel it
    /// is *silently dropped*. Counters are auxiliary telemetry — a read must
    /// never block, retry, or fail because the counter pipeline is saturated or
    /// shutting down. Called from the scoped read paths (`node`,
    /// `nodes_by_label`, `traverse`) with exactly the nodes they returned.
    pub(crate) fn bump(&self, ids: impl IntoIterator<Item = NodeId>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as i64;
        // Clone the sender out from under the mutex so we never hold the lock
        // across `try_send`. `None` once `Drop` has taken it — nothing to bump.
        // A poisoned mutex (applier panicked; poisoned-lock policy) also yields
        // `None`: bumps are auxiliary telemetry, so we silently drop them rather
        // than propagate the panic into a read path.
        let tx = self
            .inner
            .bump_tx
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned());
        if let Some(tx) = tx {
            for id in ids {
                // Full (bumper backed up) or Disconnected (shutdown) → drop.
                let _ = tx.try_send((id, now));
            }
        }
    }

    /// Auxiliary access statistics for `id`, scoped exactly like [`Db::node`]:
    /// `None` if the node is absent OR out of `scopes` (the two are
    /// indistinguishable by design); `Some(AccessStats::default())` if the node
    /// exists in scope but has never been counted. **Reading stats never
    /// bumps** — this is a pure read of the COUNTERS table gated on node
    /// existence, so callers can inspect recency without perturbing it.
    pub fn access_stats(
        &self,
        scopes: &ScopeSet,
        id: NodeId,
    ) -> Result<Option<AccessStats>, TopoError> {
        // Gate on scoped existence *without* going through `node()` — reading
        // stats must never bump, and `node()` bumps. We replicate its scope
        // filter directly against storage: `None` if absent OR out of scope
        // (indistinguishable, mirroring `node()`).
        let in_scope = self
            .inner
            .storage
            .load_node(id)?
            .is_some_and(|n| scopes.contains(n.scope));
        if !in_scope {
            return Ok(None);
        }
        Ok(Some(
            self.inner.storage.read_counter(id)?.unwrap_or_default(),
        ))
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

    /// Subscribes to the change feed, returning the `Receiver` half of a fresh
    /// bounded channel (`capacity` slots) registered with the applier. Every
    /// op the applier commits after this call is pushed as a [`ChangeEvent`]
    /// carrying a monotonic op-log `seq`.
    ///
    /// **Unscoped, by spec design.** The change feed is a *host-level*
    /// primitive that powers external consolidation/decay — it must observe
    /// every committed write regardless of scope. Unlike the scoped read APIs
    /// (`node`, `nodes_by_label`, `traverse`), it is not gated by a
    /// `ScopeSet`.
    ///
    /// **Delivery contract (best-effort, never blocks the applier):** if this
    /// subscriber's buffer is full when the applier broadcasts, the event is
    /// **DROPPED** for this subscriber — the applier never blocks on a slow
    /// consumer. The subscriber detects the resulting gap in `seq` and
    /// recovers the missing ops with [`Db::ops_since`]. A receiver that has
    /// been dropped is pruned from the registry on the next broadcast.
    /// Rejected batches, counter flushes, and rebuilds broadcast nothing;
    /// reads never produce events.
    ///
    /// A `capacity` of 0 is clamped to 1 — crossbeam's zero-capacity channels
    /// are rendezvous channels, which would silently drop nearly every event.
    ///
    /// **Anchoring a gap-free live tail:** capture the log position *before*
    /// subscribing, then backfill the window between them once:
    /// `let seq = db.current_seq()?; let rx = db.subscribe(cap);` then replay
    /// `ops_since(seq + 1)` once and dedup by `seq` against the channel. Any op
    /// committed between the two calls appears in both the replay and the live
    /// channel; deduping by `seq` collapses the overlap, and nothing in the gap
    /// is missed. This recipe is seamless across compaction too:
    /// [`current_seq`](Db::current_seq) survives an empty-but-compacted log
    /// (it falls back to the retained floor), so `ops_since(current_seq() +
    /// 1)` never spuriously returns [`TopoError::Compacted`] right after an
    /// emptying compaction — no special-casing needed at the call site.
    #[must_use]
    pub fn subscribe(&self, capacity: usize) -> Receiver<ChangeEvent> {
        let capacity = capacity.max(1);
        let (tx, rx) = bounded::<ChangeEvent>(capacity);
        // Poisoned subs registry ⇒ the applier panicked and the engine is dead
        // (poisoned-lock policy, see vector.rs). Hand back an already-disconnected
        // Receiver rather than propagating the panic: it reports `Disconnected`
        // immediately, the same terminal signal a subscriber sees after shutdown.
        match self.inner.subs.lock() {
            Ok(mut subs) => subs.push(tx),
            Err(_) => {
                let (tx, rx) = bounded(1);
                drop(tx);
                return rx;
            }
        }
        rx
    }

    /// Replays the durable op log from `since_seq` (**INCLUSIVE**), returning
    /// one [`ChangeEvent`] per op in ascending `seq` order. This is the pull
    /// side of the change feed: subscribers that dropped events (buffer full)
    /// call it to recover the gap after noticing a jump in `seq`.
    ///
    /// **Unscoped, by spec design** — same rationale as [`Db::subscribe`]: the
    /// change feed is a host-level primitive that must see every write. This
    /// is a read: it produces no events of its own.
    ///
    /// Reading below the oldest retained seq returns
    /// [`TopoError::Compacted { oldest }`](TopoError::Compacted): the requested
    /// range dips beneath the compaction floor, so a partial replay would
    /// silently drop history. The caller re-anchors from materialized state
    /// (the NODES/EDGES tables, which stay the source of truth after
    /// compaction) rather than trusting a truncated tail. An uncompacted log
    /// has a floor of 1, so any `since_seq` succeeds.
    pub fn ops_since(&self, since_seq: u64) -> Result<Vec<ChangeEvent>, TopoError> {
        let ops = self.inner.storage.read_ops(since_seq)?;
        Ok(ops
            .into_iter()
            .map(|(seq, op)| ChangeEvent {
                seq,
                op: Arc::new(op),
            })
            .collect())
    }

    /// The highest op-log seq committed so far (0 when the log has never been
    /// written). A plain storage read — no applier round-trip — so it is
    /// cheap and safe to call from any thread. Its purpose is to anchor a
    /// gap-free live tail: take it *before* [`subscribe`](Db::subscribe),
    /// then backfill with `ops_since(seq + 1)` (see `subscribe`'s anchoring
    /// recipe).
    ///
    /// Survives compaction: on an empty-but-compacted log the last OPS key is
    /// gone, but this falls back to the retained floor (`oldest_seq - 1`) so
    /// the high-water mark is never lost. The anchoring recipe's
    /// `ops_since(current_seq() + 1)` therefore never spuriously returns
    /// [`TopoError::Compacted`] right after an emptying compaction — it only
    /// returns `Compacted` for a seq genuinely below the retained floor.
    #[must_use = "the seq anchors ops_since"]
    pub fn current_seq(&self) -> Result<u64, TopoError> {
        self.inner.storage.current_seq()
    }

    /// Compacts the durable op log, dropping every entry with seq `< keep_from`
    /// and advancing the retained floor to `keep_from`. After this,
    /// [`ops_since`](Db::ops_since) below `keep_from` returns
    /// [`TopoError::Compacted`] and [`rebuild_state_from_ops`](Db::rebuild_state_from_ops)
    /// refuses (a compacted log is no longer a full history — NODES/EDGES stay
    /// the materialized source of truth).
    ///
    /// **Host-level primitive** (unscoped, like the change feed it serves).
    /// Edge behaviour mirrors `Storage::compact_ops_through`:
    /// `keep_from <= oldest` is a no-op, `keep_from > current_seq + 1` is
    /// rejected, and `keep_from == current_seq + 1` legally empties the log.
    /// Runs on the applier thread and blocks until it commits; `Closed` after
    /// shutdown, same contract as [`submit`](Db::submit).
    pub fn compact_ops(&self, keep_from: u64) -> Result<(), TopoError> {
        let (reply_tx, reply_rx) = bounded(1);
        let tx = self.sender().ok_or(TopoError::Closed)?;
        tx.send(Job::Compact {
            keep_from,
            reply: reply_tx,
        })
        .map_err(|_| TopoError::Closed)?;
        reply_rx.recv().map_err(|_| TopoError::Closed)?
    }

    /// Clones the job `Sender` out of the mutex and releases the guard before
    /// the caller does anything blocking with it. `None` once `Drop` has taken
    /// the sender. Holding the guard across a (potentially blocking) `send` on
    /// the bounded channel would needlessly serialize all submitters against
    /// each other on the mutex rather than on the channel.
    fn sender(&self) -> Option<Sender<Job>> {
        // A poisoned mutex (applier panicked; poisoned-lock policy) maps to
        // `None`, which `submit_inner`/`rebuild_state_from_ops` already turn into
        // `TopoError::Closed` — the same result as a shut-down engine.
        self.inner.tx.lock().ok().and_then(|g| g.as_ref().cloned())
    }

    fn submit_inner(&self, ops: Vec<Op>, at: Option<i64>) -> Result<AppliedBatch, TopoError> {
        let (reply_tx, reply_rx) = bounded(1);
        let tx = self.sender().ok_or(TopoError::Closed)?;
        tx.send(Job::Apply {
            ops,
            at,
            reply: reply_tx,
        })
        .map_err(|_| TopoError::Closed)?;
        reply_rx.recv().map_err(|_| TopoError::Closed)?
    }

    /// Test/inspection helper: every edge `(from, to)` currently in storage,
    /// open or closed. `#[doc(hidden)]` — callers should prefer the query
    /// layer once it exists. Full `EdgeRecord`s (props included), resolved
    /// via a bounded OUT_ADJ scan from `from`'s slot (one read transaction) —
    /// never a full-table scan.
    #[doc(hidden)]
    pub fn all_edges_between(&self, from: NodeId, to: NodeId) -> Vec<crate::state::EdgeRecord> {
        self.edges_between(from, to).unwrap_or_default()
    }

    /// Test/inspection helper: the ids of currently-open edges `(from, to)`
    /// (i.e. `valid_to.is_none()`). `#[doc(hidden)]` — see
    /// `all_edges_between`.
    #[doc(hidden)]
    pub fn open_edges_between(&self, from: NodeId, to: NodeId) -> Vec<EdgeId> {
        self.edges_between(from, to)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.valid_to.is_none())
            .map(|e| e.id)
            .collect()
    }

    /// Shared implementation for `all_edges_between`/`open_edges_between`: a
    /// bounded OUT_ADJ scan from `from`'s slot, filtered to entries whose
    /// target resolves to `to`, then fetched as full `EdgeRecord`s — all in
    /// one read transaction. A missing `from`/`to` slot (node never existed,
    /// or was removed) yields an empty result, not an error — mirrors
    /// `Db::node`'s "absence is absence" treatment of a storage miss.
    fn edges_between(
        &self,
        from: NodeId,
        to: NodeId,
    ) -> Result<Vec<crate::state::EdgeRecord>, TopoError> {
        let storage = self.storage();
        let dicts = storage.dicts.read().expect("dict lock poisoned");
        let scope_registry = storage
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");
        let tx = storage.db.begin_read().map_err(crate::error::storage_err)?;
        let node_slots = tx
            .open_table(crate::slots::NODE_SLOTS)
            .map_err(crate::error::storage_err)?;
        let Some(from_slot) = crate::slots::node_slot(&node_slots, from)? else {
            return Ok(Vec::new());
        };
        let Some(to_slot) = crate::slots::node_slot(&node_slots, to)? else {
            return Ok(Vec::new());
        };
        let out_adj = tx
            .open_table(crate::adj::OUT_ADJ)
            .map_err(crate::error::storage_err)?;
        let edges_table = tx
            .open_table(crate::storage::EDGES)
            .map_err(crate::error::storage_err)?;
        let node_ids = tx
            .open_table(crate::slots::NODE_IDS)
            .map_err(crate::error::storage_err)?;
        let mut out = Vec::new();
        for (_ty, entry) in crate::adj::read_adj(&out_adj, from_slot, None)? {
            if entry.target != to_slot {
                continue;
            }
            if let Some(rec) = crate::storage::read_edge_by_slot(
                &edges_table,
                &dicts,
                &scope_registry,
                &node_ids,
                entry.edge,
            )? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Rebuilds NODES/EDGES (and the adjacency/index tables derived from
    /// them) from the OPS log — see `Storage::rebuild_state_from_ops`. The
    /// read model is disk-resident, so readers observe the rebuilt state as
    /// soon as this returns — there is no separate in-memory snapshot to
    /// keep in step with it.
    ///
    /// The rebuild is performed *on the applier thread* (via a `Job::Rebuild`
    /// routed through the same channel as `submit`), not on the caller
    /// thread. The applier is the sole redb writer; doing the rebuild there
    /// serializes it with batch application, so `rebuild_state_from_ops` and
    /// an in-flight `apply_batch` can never interleave. Blocks until the
    /// applier replies; `Closed` after shutdown, same contract as `submit`.
    #[doc(hidden)]
    pub fn rebuild_state_from_ops(&self) -> Result<(), TopoError> {
        let (reply_tx, reply_rx) = bounded(1);
        let tx = self.sender().ok_or(TopoError::Closed)?;
        tx.send(Job::Rebuild { reply: reply_tx })
            .map_err(|_| TopoError::Closed)?;
        reply_rx.recv().map_err(|_| TopoError::Closed)?
    }

    /// Test/inspection helper: every node currently in storage, sorted by
    /// id for deterministic comparison. `#[doc(hidden)]` — see
    /// `all_edges_between`.
    #[doc(hidden)]
    pub fn debug_dump_nodes(&self) -> Vec<crate::state::NodeRecord> {
        let mut out = self
            .inner
            .storage
            .all_nodes()
            .expect("debug dump: storage read failed");
        out.sort_by_key(|n| n.id);
        out
    }

    /// Test/inspection helper: every edge currently in storage, sorted by
    /// id for deterministic comparison. `#[doc(hidden)]` — see
    /// `all_edges_between`.
    #[doc(hidden)]
    pub fn debug_dump_edges(&self) -> Vec<crate::state::EdgeRecord> {
        let mut out = self
            .inner
            .storage
            .all_edges()
            .expect("debug dump: storage read failed");
        out.sort_by_key(|e| e.id);
        out
    }
}

/// The node ids that `VectorIndex::prevalidate_dims`, `validate::prevalidate_edge_scopes`,
/// and `VectorIndex::maintain` need pre-batch storage state (scope, embedding)
/// for: `CreateEdge`'s endpoints, `SetEmbedding`'s target, and `RemoveNode`'s
/// target. A same-batch `CreateNode` for one of these ids is resolved locally
/// by each of those three functions (via their own scan of `ops`) and needs
/// no storage lookup — this only has to cover ids that might ALREADY exist in
/// storage before this batch runs.
fn ids_needing_pre_state(ops: &[Op]) -> std::collections::HashSet<NodeId> {
    let mut ids = std::collections::HashSet::new();
    for op in ops {
        match op {
            Op::CreateEdge { from, to, .. } => {
                ids.insert(*from);
                ids.insert(*to);
            }
            Op::SetEmbedding { id, .. } | Op::RemoveNode { id } => {
                ids.insert(*id);
            }
            _ => {}
        }
    }
    ids
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Shutdown order is load-bearing because the bumper thread holds a
        // *clone* of the applier `tx`. It must be, in exactly this sequence:
        //
        //   1. take+drop `bump_tx` — closes the bump channel so the bumper's
        //      `recv_timeout` loop sees `Disconnected`, does its FINAL flush
        //      (enqueuing one last `Job::BumpCounters` into the applier
        //      channel), and returns.
        //   2. join the bumper — waits for that final flush to be enqueued and
        //      for the bumper's clone of the applier `tx` to be dropped.
        //   3. take+drop `tx` — only now, with the bumper's clone gone, does
        //      the applier channel actually close.
        //   4. join the applier — its `rx.recv()` loop finally sees the closed
        //      channel (after draining the final flush) and exits.
        //
        // Reorder these and you either deadlock (drop `tx` while the bumper's
        // clone keeps the applier channel open → applier join hangs) or lose
        // the final flush (join applier before the bumper has enqueued it).
        // Shutdown must proceed even if a mutex was poisoned by an applier panic
        // (poisoned-lock policy, see vector.rs) — otherwise the host leaks the
        // applier/bumper threads on drop. Recover the guard via `into_inner`.
        self.bump_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(h) = self
            .bumper
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(h) = self
            .applier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_receiver_is_pruned_on_next_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let rx = db.subscribe(4);
        drop(rx);
        db.submit(vec![crate::Op::CreateNode {
            id: crate::NodeId::new(),
            scope: crate::Scope::Id(crate::ScopeId::new()),
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
        assert_eq!(
            db.inner.subs.lock().unwrap().len(),
            0,
            "disconnected sender must be pruned"
        );
    }
}
