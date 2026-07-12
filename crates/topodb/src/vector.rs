//! Graph-scoped vector search over per-`(model, scope)` contiguous f32 slabs.
//!
//! Each `(model, scope)` pair owns one [`Slab`]: a flat `Vec<f32>` where row
//! `i` (`ids[i]`) is the embedding at `data[i*dim .. (i+1)*dim]`. Tombstoned
//! rows keep their `data` in place (marked `ids[i] == None`) until a
//! compaction pass rebuilds the slab.
//!
//! **Locking.** The [`VectorIndex`] outer map is mutated only by the applier
//! thread (slab creation and, on rebuild, a wholesale swap). Each slab is
//! mutated in place under its own `RwLock`; searches take short read locks.
//! This is the *one* read path that is not lock-free: the spec's lock-free
//! guarantee covers snapshot/adjacency reads, while slab write locks are held
//! only for O(dim) per op on the applier, and slab read locks only for the
//! duration of a single `top_k` scan.
//!
//! **Poisoned-lock policy.** Std `RwLock`/`Mutex` poisoning can only originate
//! from a panic on the applier thread — the sole writer of the slabs map, the
//! per-slab write locks, and the subs registry. After such a panic the engine
//! is dead: `submit` already returns [`TopoError::Closed`] (its channel is
//! gone). This module makes the READ paths agree rather than propagating the
//! panic into the host: search-path lock acquisitions go through
//! [`read_or_closed`], which maps a poisoned lock to `Err(Closed)`. Applier-side
//! lock acquisitions keep `unwrap()` (a poison there means THIS thread already
//! panicked — unreachable). Shutdown (`Drop for Inner` in `db.rs`) recovers
//! poisoned mutexes via `into_inner` so threads are still joined.

use crate::db::Db;
use crate::error::TopoError;
use crate::ids::{NodeId, Scope, ScopeSet};
use crate::op::Op;
use crate::state::NodeRecord;
use crate::storage::Storage;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// Acquire a read lock, mapping a poisoned lock to [`TopoError::Closed`].
///
/// Poisoning is only reachable if the applier thread panicked while holding the
/// write lock — at which point the engine is already dead — so on the read path
/// the correct answer is `Closed`, not a propagated panic. See the module-level
/// poisoned-lock policy.
pub(crate) fn read_or_closed<T>(
    l: &std::sync::RwLock<T>,
) -> Result<std::sync::RwLockReadGuard<'_, T>, TopoError> {
    l.read().map_err(|_| TopoError::Closed)
}

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

    /// Cosine-score every live row against `query` (skipping zero-norm rows and
    /// any row filtered out by `filter`), returning the top `k` by descending
    /// score. Callers guarantee `query.len() == self.dim`.
    pub(crate) fn top_k(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&HashSet<NodeId>>,
    ) -> Vec<(NodeId, f32)> {
        let mut hits: Vec<(NodeId, f32)> = Vec::new();
        for (row, slot) in self.ids.iter().enumerate() {
            let Some(id) = slot else { continue };
            if let Some(f) = filter {
                if !f.contains(id) {
                    continue;
                }
            }
            let start = row * self.dim;
            let vec = &self.data[start..start + self.dim];
            if let Some(score) = cosine(vec, query) {
                hits.push((*id, score));
            }
        }
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        hits.truncate(k);
        hits
    }
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// All slabs, keyed by (model, scope). Two-level locking: the outer map is
/// only mutated by the applier (slab creation); each slab is mutated in
/// place under its own RwLock — searches take short read locks. This is the
/// one read path that is not lock-free; the spec's lock-free guarantee
/// covers snapshot/adjacency reads, and slab write locks are held only for
/// O(dim) per op on the applier.
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
    /// `Rejected` if `q.k == 0` or `q.vector` is empty. Collects the slabs
    /// whose key matches `(q.model, scope ∈ q.scopes)`, skips any whose dim
    /// differs from the query (defensive — pre-validation keeps a live slab's
    /// dim uniform), scores each under a short read lock with the optional
    /// candidate filter, merges, sorts descending, and truncates to `k`. Ids
    /// are mapped to `NodeRecord` through a single storage read transaction;
    /// an id storage no longer carries (the slab can be momentarily
    /// ahead of/behind storage between locks) is dropped — harmless. Result
    /// nodes are bumped (access counters, Task 4).
    ///
    /// Returns [`TopoError::Closed`] if a slab lock is poisoned — reachable only
    /// after the applier thread has panicked (the engine is already dead); see
    /// the module-level poisoned-lock policy.
    pub fn search_vector(&self, q: &VectorQuery) -> Result<Vec<(NodeRecord, f32)>, TopoError> {
        if q.k == 0 || q.vector.is_empty() {
            return Err(TopoError::Rejected(
                "vector search requires k > 0 and a non-empty query vector".into(),
            ));
        }
        let filter: Option<HashSet<NodeId>> =
            q.candidates.as_ref().map(|c| c.iter().copied().collect());

        let vectors = self.vectors();
        let mut merged: Vec<(NodeId, f32)> = Vec::new();
        {
            let slabs = read_or_closed(&vectors.slabs)?;
            for ((model, scope), arc) in slabs.iter() {
                if model != &q.model || !q.scopes.contains(*scope) {
                    continue;
                }
                let slab = read_or_closed(arc)?;
                if slab.dim != q.vector.len() {
                    continue;
                }
                merged.extend(slab.top_k(&q.vector, q.k, filter.as_ref()));
            }
        }
        merged.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        merged.truncate(q.k);

        let ids: HashSet<NodeId> = merged.iter().map(|(id, _)| *id).collect();
        let by_id = self.storage().load_nodes(&ids).unwrap_or_default();
        let mut out: Vec<(NodeRecord, f32)> = Vec::with_capacity(merged.len());
        for (id, score) in merged {
            if let Some(rec) = by_id.get(&id) {
                out.push((rec.clone(), score));
            }
        }
        self.bump(out.iter().map(|(n, _)| n.id));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ScopeId;

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
