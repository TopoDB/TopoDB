//! Per-`(model, scope)` RAM slab index — write-only as of Task 5.
//!
//! Each `(model, scope)` pair owns one [`Slab`]: a flat `Vec<f32>` where row
//! `i` (`ids[i]`) is the embedding at `data[i*dim .. (i+1)*dim]`. Tombstoned
//! rows keep their `data` in place (marked `ids[i] == None`) until a
//! compaction pass rebuilds the slab.
//!
//! **Task 5: the read cutover.** `Db::search_vector` (below) no longer reads
//! this index — it reads the v4 clustered `vectors`/`embedding_ref` tables
//! via [`crate::vector_store::search_scan`] instead. The slab index stays
//! alive and is still fully maintained by the applier thread (dim
//! pre-validation via [`VectorIndex::prevalidate_dims`], and
//! upsert/tombstone/compact via [`VectorIndex::maintain`]) — it's just no
//! longer consulted by any read path.
//!
//! **Locking.** The [`VectorIndex`] outer map is mutated only by the applier
//! thread (slab creation and, on rebuild, a wholesale swap). Each slab is
//! mutated in place under its own `RwLock`, held only for O(dim) per op on
//! the applier — no read path takes these locks anymore.
//!
//! **Poisoned-lock policy.** Std `RwLock`/`Mutex` poisoning can only originate
//! from a panic on the applier thread — the sole writer of the slabs map, the
//! per-slab write locks, and the subs registry. After such a panic the engine
//! is dead: `submit` already returns [`TopoError::Closed`] (its channel is
//! gone). Applier-side lock acquisitions keep `unwrap()` (a poison there
//! means THIS thread already panicked — unreachable). Shutdown (`Drop for
//! Inner` in `db.rs`) recovers poisoned mutexes via `into_inner` so threads
//! are still joined. Now that no read path takes a slab lock, the
//! `read_or_closed`-maps-poison-to-`Closed` mapping is exercised only by this
//! module's own tests, below, directly against a bare `VectorIndex`.

use crate::db::Db;
use crate::error::{storage_err, TopoError};
use crate::ids::{NodeId, Scope, ScopeSet};
use crate::op::Op;
use crate::state::NodeRecord;
use crate::storage::{read_node_by_slot, Storage, EMBEDDINGS, NODES};
use crate::vector_store::{search_scan, OrderedScore};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// A vector-search request: cosine-rank the embeddings under `model` within
/// `scopes` against `vector`, returning the top `k`.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub scopes: ScopeSet,
    pub model: String,
    pub vector: Vec<f32>,
    pub k: usize,
    /// Restrict scoring to these nodes (e.g. a traversal result). None = whole scope set.
    pub candidates: Option<Vec<NodeId>>,
}

/// One contiguous slab: row i = ids[i] ↔ data[i*dim .. (i+1)*dim].
/// Tombstoned rows have ids[i] == None (data left in place until compaction).
pub(crate) struct Slab {
    pub(crate) dim: usize,
    pub(crate) ids: Vec<Option<NodeId>>,
    pub(crate) data: Vec<f32>,
    pub(crate) row_of: HashMap<NodeId, usize>,
    pub(crate) dead: usize,
}

impl Slab {
    pub(crate) fn new(dim: usize) -> Slab {
        Slab {
            dim,
            ids: Vec::new(),
            data: Vec::new(),
            row_of: HashMap::new(),
            dead: 0,
        }
    }

    /// Tombstone any existing row for `id`, then append `v` as a fresh row.
    ///
    /// A slab's `dim` is fixed for the lifetime of its live rows. If the slab
    /// currently holds no live rows (freshly created, or every row
    /// tombstoned) and `v`'s length differs, the slab is re-dimensioned — the
    /// only case where `dim` changes. When there ARE live rows, the applier's
    /// dim pre-validation guarantees `v.len() == dim`, so the debug assert
    /// below can never fire for a validated batch.
    pub(crate) fn upsert(&mut self, id: NodeId, v: &[f32]) {
        let live = self.ids.len() - self.dead;
        if v.len() != self.dim && live == 0 {
            self.ids.clear();
            self.data.clear();
            self.row_of.clear();
            self.dead = 0;
            self.dim = v.len();
        }
        debug_assert_eq!(
            v.len(),
            self.dim,
            "upsert vector length must match slab dim"
        );
        // Tombstone the prior row (if any) before appending the new one.
        self.tombstone(id);
        let row = self.ids.len();
        self.ids.push(Some(id));
        self.data.extend_from_slice(v);
        self.row_of.insert(id, row);
    }

    /// Mark `id`'s row dead (no-op if absent). Data stays in place; the row is
    /// reclaimed by `maybe_compact`.
    pub(crate) fn tombstone(&mut self, id: NodeId) {
        if let Some(row) = self.row_of.remove(&id) {
            if self.ids[row].is_some() {
                self.ids[row] = None;
                self.dead += 1;
            }
        }
    }

    /// Rebuild `ids`/`data`/`row_of` dropping dead rows, but only once the
    /// dead rows outnumber the live ones (`dead > live`) — amortising the O(n)
    /// rebuild against the churn that produced the tombstones.
    pub(crate) fn maybe_compact(&mut self) {
        let live = self.ids.len() - self.dead;
        if self.dead <= live {
            return;
        }
        let mut new_ids: Vec<Option<NodeId>> = Vec::with_capacity(live);
        let mut new_data: Vec<f32> = Vec::with_capacity(live * self.dim);
        let mut new_row_of: HashMap<NodeId, usize> = HashMap::with_capacity(live);
        for (row, slot) in self.ids.iter().enumerate() {
            if let Some(id) = slot {
                let new_row = new_ids.len();
                new_ids.push(Some(*id));
                new_data.extend_from_slice(&self.data[row * self.dim..(row + 1) * self.dim]);
                new_row_of.insert(*id, new_row);
            }
        }
        self.ids = new_ids;
        self.data = new_data;
        self.row_of = new_row_of;
        self.dead = 0;
    }
}

/// All slabs, keyed by (model, scope). Two-level locking: the outer map is
/// only mutated by the applier (slab creation); each slab is mutated in
/// place under its own RwLock. Write-only as of Task 5 — see the module doc.
pub(crate) struct VectorIndex {
    // Type shape is fixed by the task spec; the nested locks are the whole
    // point (outer map guards slab creation, inner lock guards each slab).
    #[allow(clippy::type_complexity)]
    pub(crate) slabs: RwLock<HashMap<(String, Scope), Arc<RwLock<Slab>>>>,
}

impl VectorIndex {
    pub(crate) fn new() -> VectorIndex {
        VectorIndex {
            slabs: RwLock::new(HashMap::new()),
        }
    }

    /// Fold every embedded node into a fresh index by scanning the EMBEDDINGS
    /// table and joining each row's node record for id/scope. Used at `Db`
    /// open and by [`rebuild_from`](VectorIndex::rebuild_from). This is the
    /// ONLY remaining open-time scan (SP2 removes it) — see
    /// `Storage::all_embeddings_with_scope`.
    // SP2: last open-time scan
    pub(crate) fn from_storage(storage: &Storage) -> Result<VectorIndex, TopoError> {
        let idx = VectorIndex::new();
        for (model, scope, id, vector) in storage.all_embeddings_with_scope()? {
            idx.upsert(&model, scope, id, &vector);
        }
        Ok(idx)
    }

    /// The slab dim for `(model, scope)`, but only while the slab holds live
    /// rows — an empty or fully-tombstoned slab imposes no dim constraint (its
    /// `dim` is re-settable via the next `upsert`), so this reports `None`.
    pub(crate) fn dim_of(&self, model: &str, scope: Scope) -> Option<usize> {
        // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
        let slabs = self.slabs.read().unwrap();
        let arc = slabs.get(&(model.to_string(), scope))?;
        // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
        let slab = arc.read().unwrap();
        let live = slab.ids.len() - slab.dead;
        (live > 0).then_some(slab.dim)
    }

    /// Upsert `id`'s embedding into `(model, scope)`, creating the slab on
    /// first use.
    pub(crate) fn upsert(&self, model: &str, scope: Scope, id: NodeId, v: &[f32]) {
        let key = (model.to_string(), scope);
        // Fast path: slab already exists — take only a read lock on the outer
        // map, then a write lock on the one slab.
        let arc = {
            // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
            let slabs = self.slabs.read().unwrap();
            slabs.get(&key).cloned()
        };
        let arc = match arc {
            Some(a) => a,
            None => {
                // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
                let mut slabs = self.slabs.write().unwrap();
                slabs
                    .entry(key)
                    .or_insert_with(|| Arc::new(RwLock::new(Slab::new(v.len()))))
                    .clone()
            }
        };
        // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
        arc.write().unwrap().upsert(id, v);
    }

    /// Tombstone `id` in `(model, scope)` (no-op if the slab or row is absent).
    pub(crate) fn tombstone(&self, model: &str, scope: Scope, id: NodeId) {
        let arc = {
            // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
            let slabs = self.slabs.read().unwrap();
            slabs.get(&(model.to_string(), scope)).cloned()
        };
        if let Some(arc) = arc {
            // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
            arc.write().unwrap().tombstone(id);
        }
    }

    /// Compaction hook for a single touched slab (see `Slab::maybe_compact`).
    pub(crate) fn maybe_compact(&self, model: &str, scope: Scope) {
        let arc = {
            // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
            let slabs = self.slabs.read().unwrap();
            slabs.get(&(model.to_string(), scope)).cloned()
        };
        if let Some(arc) = arc {
            // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
            arc.write().unwrap().maybe_compact();
        }
    }

    /// Dim pre-validation, run by the applier BEFORE `apply_batch` so a
    /// violation leaves storage untouched (atomic with the rest of the batch).
    /// For each `SetEmbedding`, the vector must be non-empty, its length must
    /// equal the existing live-slab dim for `(model, node.scope)`, and must be
    /// consistent for that key across the batch itself. The node's scope comes
    /// from a same-batch `CreateNode` if present, otherwise from `pre` (a
    /// storage read of pre-batch node state taken by the applier before
    /// `apply_batch`, keyed by every id this batch might reference); a
    /// `SetEmbedding` for a node that exists nowhere is left for `apply_batch`
    /// to reject.
    pub(crate) fn prevalidate_dims(
        &self,
        pre: &HashMap<NodeId, NodeRecord>,
        ops: &[Op],
    ) -> Result<(), TopoError> {
        let mut created_scope: HashMap<NodeId, Scope> = HashMap::new();
        let mut batch_dims: HashMap<(String, Scope), usize> = HashMap::new();
        for op in ops {
            match op {
                Op::CreateNode { id, scope, .. } => {
                    created_scope.insert(*id, *scope);
                }
                Op::SetEmbedding { id, model, vector } => {
                    // A zero-dim embedding is meaningless on its own AND
                    // poisons the `(model, scope)` slab: it fixes the slab's
                    // dim at 0, after which every real embedding under that
                    // key is rejected as a dim conflict, permanently. Reject
                    // it here — symmetric with `search_vector`, which already
                    // refuses an empty query vector.
                    if vector.is_empty() {
                        return Err(TopoError::Rejected(format!(
                            "embedding for model {model:?} must have at least one dimension"
                        )));
                    }
                    let scope = match created_scope.get(id) {
                        Some(s) => *s,
                        None => match pre.get(id) {
                            Some(n) => n.scope,
                            // Node exists nowhere — apply_batch rejects it.
                            None => continue,
                        },
                    };
                    let dim = vector.len();
                    let key = (model.clone(), scope);
                    match batch_dims.get(&key) {
                        Some(&d) if d != dim => {
                            return Err(TopoError::Rejected(format!(
                                "inconsistent embedding dim for model {model:?} within batch: {d} vs {dim}"
                            )));
                        }
                        Some(_) => {}
                        None => {
                            batch_dims.insert(key, dim);
                        }
                    }
                    if let Some(existing) = self.dim_of(model, scope) {
                        if existing != dim {
                            return Err(TopoError::Rejected(format!(
                                "embedding dim {dim} does not match existing slab dim {existing} for model {model:?}"
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Slab maintenance, run by the applier AFTER `apply_batch` succeeds,
    /// using `pre` (a storage read of pre-batch node state taken by the
    /// applier BEFORE `apply_batch` — old state is no longer readable from
    /// storage once `apply_batch` has committed) for old state. The batch's
    /// own ops are walked in order so same-batch `CreateNode`/`SetEmbedding`
    /// chains resolve against each other before falling back to `pre`.
    ///
    /// - `SetEmbedding` → if the node's OLD embedding was under a *different*
    ///   model, tombstone it in that slab; then upsert into `(model, scope)`
    ///   (a same-model re-embed is handled by `upsert` tombstoning the old row
    ///   in place).
    /// - `RemoveNode` → if the old record had an embedding `(m, _)`, tombstone
    ///   in `(m, scope)`.
    ///
    /// After the walk, every touched slab is offered to `maybe_compact`.
    pub(crate) fn maintain(&self, pre: &HashMap<NodeId, NodeRecord>, resolved: &[Op]) {
        // Per-node evolving `(scope, current embedding model)`, seeded lazily
        // from `pre` and updated as the batch's ops are replayed.
        let mut local: HashMap<NodeId, (Scope, Option<String>)> = HashMap::new();
        let mut touched: HashSet<(String, Scope)> = HashSet::new();

        for op in resolved {
            match op {
                Op::CreateNode { id, scope, .. } => {
                    // CreateNode never carries an embedding.
                    local.insert(*id, (*scope, None));
                }
                Op::SetEmbedding { id, model, vector } => {
                    let (scope, old_model) = match local.get(id) {
                        Some(s) => s.clone(),
                        None => match pre.get(id) {
                            Some(n) => (n.scope, n.embedding.as_ref().map(|(m, _)| m.clone())),
                            None => continue,
                        },
                    };
                    if let Some(om) = &old_model {
                        if om != model {
                            self.tombstone(om, scope, *id);
                            touched.insert((om.clone(), scope));
                        }
                    }
                    self.upsert(model, scope, *id, vector);
                    touched.insert((model.clone(), scope));
                    local.insert(*id, (scope, Some(model.clone())));
                }
                Op::RemoveNode { id } => {
                    let (scope, old_model) = match local.get(id) {
                        Some(s) => s.clone(),
                        None => match pre.get(id) {
                            Some(n) => (n.scope, n.embedding.as_ref().map(|(m, _)| m.clone())),
                            None => continue,
                        },
                    };
                    if let Some(om) = &old_model {
                        self.tombstone(om, scope, *id);
                        touched.insert((om.clone(), scope));
                    }
                    local.insert(*id, (scope, None));
                }
                _ => {}
            }
        }

        for (model, scope) in touched {
            self.maybe_compact(&model, scope);
        }
    }

    /// Rebuild the whole index from `storage`, swapping the inner map in
    /// place (the applier holds this behind the shared `Arc`, so the `Arc`
    /// itself must not be replaced — only its contents). Used by the
    /// `Job::Rebuild` arm after `Storage::rebuild_state_from_ops` completes.
    pub(crate) fn rebuild_from(&self, storage: &Storage) -> Result<(), TopoError> {
        let fresh = VectorIndex::from_storage(storage)?;
        // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
        let new_map = fresh.slabs.into_inner().unwrap();
        // sole writer — a poisoned lock here means THIS thread already panicked; unreachable.
        *self.slabs.write().unwrap() = new_map;
        Ok(())
    }
}

impl Db {
    /// Cosine vector search under one `model`, scoped to `q.scopes`.
    ///
    /// `Rejected` if `q.k == 0` or `q.vector` is empty. Task 5: reads the v4
    /// clustered `vectors`/`embedding_ref` tables (`vector_store::search_scan`)
    /// inside ONE `begin_read` transaction that also resolves the winning
    /// slots straight to `NodeRecord`s via NODES/EMBEDDINGS — mirrors
    /// `search_text`'s single-hop read (`fts.rs`), no separate snapshot. The
    /// RAM slab index is write-only now (see the module doc); nothing here
    /// touches it.
    ///
    /// **Tie-break seam.** `search_scan` bounds each `(model, scope)`
    /// cluster's scan through a k-heap that conservatively retains ties at
    /// the boundary score rather than picking a winner by slot (creation)
    /// order — slot order is NOT `NodeId`/ULID order. This function applies
    /// the FINAL sort — score desc, `NodeId` asc, matching the old
    /// `Slab::top_k`'s tie-break — only AFTER every surviving slot has been
    /// resolved to its `NodeId`, and truncates to `k` only then. Doing the
    /// tie-break before resolution (i.e. inside the heap, by slot) would risk
    /// silently keeping the wrong side of a same-score tie.
    ///
    /// A resolved id storage no longer carries (removed between the scan and
    /// the resolve, both inside the same transaction so this is only a
    /// theoretical race with a concurrent writer's *next* transaction) is
    /// dropped — harmless. Result nodes are bumped (access counters).
    pub fn search_vector(&self, q: &VectorQuery) -> Result<Vec<(NodeRecord, f32)>, TopoError> {
        if q.k == 0 || q.vector.is_empty() {
            return Err(TopoError::Rejected(
                "vector search requires k > 0 and a non-empty query vector".into(),
            ));
        }

        let storage = self.storage();
        let tx = storage.db.begin_read().map_err(storage_err)?;
        let dicts = storage.dicts.read().expect("dict lock poisoned");
        let scope_registry = storage
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");

        let hits = search_scan(&tx, &dicts, &scope_registry, q)?;

        let nodes = tx.open_table(NODES).map_err(storage_err)?;
        let embeddings = tx.open_table(EMBEDDINGS).map_err(storage_err)?;

        let mut out: Vec<(NodeRecord, f32)> = Vec::with_capacity(hits.len());
        for (slot, score) in hits {
            if let Some(rec) =
                read_node_by_slot(&nodes, &embeddings, &dicts, &scope_registry, slot)?
            {
                // Defensive only, not load-bearing for isolation: `search_scan`
                // already restricts its scan to `q.scopes`'s own interned
                // scope ids (and, on the candidates fast path, checks the
                // resolved scope directly) — mirrors `search_text`'s
                // identical defensive re-check.
                if q.scopes.contains(rec.scope) {
                    out.push((rec, score));
                }
            }
        }
        out.sort_by(|a, b| {
            OrderedScore(b.1)
                .cmp(&OrderedScore(a.1))
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        out.truncate(q.k);
        self.bump(out.iter().map(|(n, _)| n.id));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ScopeId;

    /// Acquire a read lock, mapping a poisoned lock to [`TopoError::Closed`]
    /// — the mapping every read path used to apply before Task 5 (see the
    /// module-level poisoned-lock policy). No production caller anymore
    /// (nothing reads the slab index), so this now lives here, exercised
    /// directly against a bare `VectorIndex` by the two tests below.
    fn read_or_closed<T>(
        l: &std::sync::RwLock<T>,
    ) -> Result<std::sync::RwLockReadGuard<'_, T>, TopoError> {
        l.read().map_err(|_| TopoError::Closed)
    }

    #[test]
    fn poisoned_slab_lock_maps_to_closed_not_panic() {
        let idx = VectorIndex::new();
        let scope = Scope::Id(ScopeId::new());
        // Two distinct slabs (different scopes), so poisoning one leaves the
        // other's lock — and the outer map lock — untouched.
        idx.upsert("m1", scope, NodeId::new(), &[1.0, 0.0]);
        let other_scope = Scope::Id(ScopeId::new());
        idx.upsert("m1", other_scope, NodeId::new(), &[1.0, 0.0]);

        let poisoned_arc = {
            // sole writer in this test — no applier thread involved.
            let slabs = idx.slabs.read().unwrap();
            slabs.get(&("m1".to_string(), scope)).unwrap().clone()
        };
        // Poison ONLY this slab's inner lock from a scratch thread, holding its
        // write guard across the panic.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = poisoned_arc.write().unwrap();
            panic!("poison this slab only");
        }));
        assert!(r.is_err());
        // Semicolon so the match's temporary `Result` (which may hold a guard
        // borrowing `poisoned_arc`) is dropped before it at end of scope.
        match read_or_closed(&poisoned_arc) {
            Err(TopoError::Closed) => {}
            other => panic!("expected Closed, got {:?}", other.map(|_| ())),
        };

        // The OTHER slab (and the outer map lock) is unaffected by this scope's
        // poison — the poisoned-lock policy is per-slab, not engine-wide.
        let healthy_arc = {
            let slabs = idx.slabs.read().unwrap();
            slabs.get(&("m1".to_string(), other_scope)).unwrap().clone()
        };
        assert!(read_or_closed(&healthy_arc).is_ok());
    }

    #[test]
    fn poisoned_outer_lock_maps_to_closed_not_panic() {
        let idx = VectorIndex::new();
        // Poison the outer map lock from a scratch thread.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = idx.slabs.write().unwrap();
            panic!("poison it");
        }));
        assert!(r.is_err());
        // Semicolon so the match's temporary `Result` (which may hold a guard
        // borrowing `idx.slabs`) is dropped before `idx` at end of scope.
        match read_or_closed(&idx.slabs) {
            Err(TopoError::Closed) => {}
            other => panic!("expected Closed, got {:?}", other.map(|_| ())),
        };
    }
}
