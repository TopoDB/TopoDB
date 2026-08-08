//! v4 clustered vector storage (Task 3 layout; Task 5 read path): `vectors`
//! keyed by `(model, scope, slot)` — a fixed-width, BE-sortable key so a
//! single model+scope's rows are one contiguous, boundedly-scannable range —
//! plus `embedding_ref`, a slot-keyed pointer to a node's CURRENT `(model,
//! scope)` so a re-embed or removal can find (and delete) the OLD `vectors`
//! row in O(1) rather than scanning.
//!
//! Dual-written alongside the still-authoritative v3 `EMBEDDINGS` table
//! (`storage.rs`) by `apply_op`'s `SetEmbedding`/`RemoveNode` arms.
//! [`search_scan`] is the Task 5 read cutover: `Db::search_vector`
//! (`vector.rs`) now reads THESE tables — the in-RAM slab index is
//! write-only from here on (still maintained by the applier for dim
//! pre-validation, but nothing reads it).
use crate::codec::{frame_value, unframe_value};
use crate::dict::{DictKind, Dicts};
use crate::error::{storage_err, TopoError};
use crate::hnsw;
use crate::scopes::ScopeRegistry;
use crate::slots::{node_slot, NODE_SLOTS};
use crate::vector::VectorQuery;
use redb::{ReadTransaction, ReadableTable, Table, TableDefinition};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// Clustered embedding rows: `vector_key(model, scope, slot)` -> framed
/// postcard `(f32, Vec<i8>) = (scale, codes)` (SQ8 quantized, format v8).
pub(crate) const VECTORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("vectors");
/// Per-node pointer to its current `(model, scope)`: 8-byte BE node slot ->
/// postcard `(u32, u32)`. Small fixed-size value — not framed; framing exists
/// to lz4-compress large payloads and a `(u32, u32)` never crosses that
/// threshold. Lets `put_vector`/`remove_vector`/`read_qvec_by_slot` find a
/// node's `vectors` row (old or current) in O(1) instead of a scan.
pub(crate) const EMBEDDING_REF: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("embedding_ref");

fn slot_key(slot: u64) -> [u8; 8] {
    slot.to_be_bytes()
}

/// Fixed-width 16-byte key: model (4-byte BE) ++ scope (4-byte BE) ++ slot
/// (8-byte BE). The `(model, scope)` prefix sorts first, so every row for a
/// given model+scope is one contiguous range — see `vector_prefix`. Fixed
/// width (unlike e.g. `prop_index.rs`'s variable-length keys) means a prefix
/// range scan needs no trailing length check to exclude a longer sibling key.
pub(crate) fn vector_key(model: u32, scope: u32, slot: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&model.to_be_bytes());
    k[4..8].copy_from_slice(&scope.to_be_bytes());
    k[8..16].copy_from_slice(&slot.to_be_bytes());
    k
}

/// The 8-byte `(model, scope)` prefix shared by every `vector_key` row in
/// that cluster. Bound a `range` scan with `vector_prefix(..) ++ 0u64` (or
/// `u64::MAX` on the high end) to enumerate — or prove empty — exactly one
/// model+scope's rows without touching any other cluster. The read path off
/// the RAM slab — see [`search_scan`]'s per-scope range scan.
pub(crate) fn vector_prefix(model: u32, scope: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0..4].copy_from_slice(&model.to_be_bytes());
    k[4..8].copy_from_slice(&scope.to_be_bytes());
    k
}

fn encode_ref(model: u32, scope: u32) -> Result<Vec<u8>, TopoError> {
    postcard::to_allocvec(&(model, scope)).map_err(|e| TopoError::Encoding(e.to_string()))
}

/// `pub(crate)` so `db.rs`'s `debug_dump_embedding_ref` seam can decode
/// `EMBEDDING_REF` rows with the exact same logic `put_vector`/
/// `remove_vector`/`read_qvec_by_slot` use, rather than a second,
/// possibly-drifting decoder.
pub(crate) fn decode_ref(bytes: &[u8]) -> Result<(u32, u32), TopoError> {
    postcard::from_bytes(bytes).map_err(|e| TopoError::Encoding(e.to_string()))
}

/// Writes `v` under `(model, scope, slot)`, first consulting `refs[slot]`
/// (the node's PRIOR ref, if it has one) so a re-embed under a DIFFERENT
/// model deletes the old `vectors` row rather than leaking an orphan.
/// Same-model re-embeds land on the identical key (scope and slot are
/// immutable for a node's lifetime) and simply overwrite in place — no
/// separate delete needed. `refs[slot]` is then written/overwritten to the
/// new `(model, scope)`.
pub(crate) fn put_vector(
    vectors: &mut Table<'_, &'static [u8], &'static [u8]>,
    refs: &mut Table<'_, &'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    slot: u64,
    v: &[f32],
) -> Result<(), TopoError> {
    let rk = slot_key(slot);
    // Convert the read to an owned `Option<(u32, u32)>` FIRST so the
    // `AccessGuard` borrowing `refs` drops before the mutable `insert` calls
    // below — same pattern as `storage.rs`'s `check_or_pin_dim`.
    let old: Option<(u32, u32)> = match refs.get(rk.as_slice()).map_err(storage_err)? {
        Some(g) => Some(decode_ref(g.value())?),
        None => None,
    };
    if let Some((old_model, old_scope)) = old {
        if old_model != model {
            vectors
                .remove(vector_key(old_model, old_scope, slot).as_slice())
                .map_err(storage_err)?;
        } else {
            // Same-model re-embed: must land on the identical `vector_key`
            // (see this function's doc comment), which is only true if the
            // node's scope hasn't moved. A node's scope is immutable for its
            // whole lifetime (no `Op` ever changes it), so this can never
            // fire outside of a bug that violates that invariant — self-
            // enforcing rather than merely assumed, so a future regression
            // trips a debug assert instead of silently leaking an orphan
            // `vectors` row under the OLD scope.
            debug_assert_eq!(
                old_scope, scope,
                "node scope is immutable; a same-model re-embed can never move scopes"
            );
        }
    }
    let (scale, codes) = crate::quant::quantize(v);
    let raw =
        postcard::to_allocvec(&(scale, codes)).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let framed = frame_value(raw);
    vectors
        .insert(vector_key(model, scope, slot).as_slice(), framed.as_slice())
        .map_err(storage_err)?;
    refs.insert(rk.as_slice(), encode_ref(model, scope)?.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

/// Clears both tables' rows for `slot` — a no-op if the node was never
/// embedded (no `refs[slot]` row to begin with).
pub(crate) fn remove_vector(
    vectors: &mut Table<'_, &'static [u8], &'static [u8]>,
    refs: &mut Table<'_, &'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<(), TopoError> {
    let rk = slot_key(slot);
    let old: Option<(u32, u32)> = match refs.get(rk.as_slice()).map_err(storage_err)? {
        Some(g) => Some(decode_ref(g.value())?),
        None => None,
    };
    if let Some((model, scope)) = old {
        vectors
            .remove(vector_key(model, scope, slot).as_slice())
            .map_err(storage_err)?;
        refs.remove(rk.as_slice()).map_err(storage_err)?;
    }
    Ok(())
}

/// Looks up a node's current embedding by its dense slot — `Ok(None)` if the
/// node has never been embedded (empty-key doctrine: an absent `refs[slot]`
/// row is an ordinary, expected miss, not an error). A `refs[slot]` row
/// whose `vectors` row is missing is corruption (`TopoError::Encoding`),
/// never a silent `None` — the two rows are always written/removed together
/// by `put_vector`/`remove_vector`, mirroring `storage.rs::read_node`'s
/// ULID-mapping-without-record-row two-cause miss split. Used by
/// `storage.rs`'s Task-3 consistency cross-check test and by
/// [`search_scan`]'s candidates fast path (one O(1) lookup per candidate
/// instead of a range scan).
// The `(u32, u32, f32, Vec<i8>) = (model, scope, scale, codes)` return shape
// is a pinned cross-wave interface contract (T4/T5 consume it verbatim,
// per the F8 task briefs) — not simplified into a named type here.
#[allow(clippy::type_complexity)]
pub(crate) fn read_qvec_by_slot(
    vectors: &impl ReadableTable<&'static [u8], &'static [u8]>,
    refs: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<(u32, u32, f32, Vec<i8>)>, TopoError> {
    let rk = slot_key(slot);
    let Some(g) = refs.get(rk.as_slice()).map_err(storage_err)? else {
        return Ok(None);
    };
    let (model, scope) = decode_ref(g.value())?;
    drop(g);
    match vectors
        .get(vector_key(model, scope, slot).as_slice())
        .map_err(storage_err)?
    {
        Some(v) => {
            let raw = unframe_value(v.value())?;
            let (scale, codes): (f32, Vec<i8>) =
                postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some((model, scope, scale, codes)))
        }
        None => Err(TopoError::Encoding(
            "read_qvec_by_slot: embedding_ref present but vectors row missing".into(),
        )),
    }
}

/// A total order over `f32` scores via [`f32::total_cmp`] — `f32` isn't
/// `Ord`, so the heap wraps every score in this newtype.
///
/// **NaN finding (historical, doubly so under v8):** this doc comment
/// predates two changes that have since retired its original worry. First
/// (pre-v8, F8 review): the write path (`storage.rs::apply_op`'s
/// `SetEmbedding` arm) now rejects any non-finite component outright
/// (currently storage.rs:2903), so a NaN/±Infinity component can no longer
/// be written by a live applier — only pre-v4-era migrated rows could ever
/// carry one. Second (format v8, SQ8): scores are no longer f32 dot/sqrt
/// over raw components — `cosine_q` (`quant.rs`) accumulates in `i64` over
/// quantized `i8` codes and divides by `sqrt` of two norms that are each
/// ≥ 1 whenever nonzero, so its `f32` result CANNOT be NaN or ±Infinity by
/// construction. The one v8 behavioral change worth noting: an all-NaN
/// query vector quantizes to all-zero codes (NaN components never set
/// `maxabs`, so `quantize` returns the zero encoding), and `cosine_q`
/// returns `None` — not a score — against an all-zero side, so the query
/// yields an EMPTY result rather than reaching this heap at all. Pre-v8,
/// the old merge sort's `partial_cmp(..).unwrap_or(Equal)` would have let
/// such a query's NaN scores rank arbitrarily and return rows anyway; the
/// v8 behavior (empty result) is deliberate and deterministic, a strict
/// improvement over that arbitrary ordering, not a regression. `total_cmp`
/// is kept regardless — it's the correct way to give `f32` a total order
/// for the heap, and a second line of defense should a future scoring path
/// ever reintroduce a non-finite score. No test exercises a NaN score on
/// either path, since none is currently reachable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OrderedScore(pub(crate) f32);
impl Eq for OrderedScore {}
impl PartialOrd for OrderedScore {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedScore {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Push `(score, slot)` onto a `k`-bounded min-heap, evicting the current
/// worst SCORE GROUP (every element tied at the heap's minimum score) as one
/// atomic unit, and only when doing so is UNAMBIGUOUS — i.e. only when the
/// heap still holds at least `k` elements after the whole group is removed.
/// If it wouldn't (some, but not provably all, of the tied group is needed
/// to fill out `k`), the entire group is put back and the heap is left
/// oversized rather than guessing which members to keep.
///
/// **Tie-break seam.** The public contract (see `vector.rs`'s
/// `Db::search_vector`) is `(score desc, NodeId asc)`, but this heap only
/// ever sees `(score, slot)` — `slot` is creation/allocation order, NOT
/// `NodeId`/ULID order (same-millisecond ULIDs randomize their relative
/// slot). So when several candidates tie at the score that is about to fall
/// off the boundary, this heap CANNOT know which of them the eventual
/// `NodeId`-order tie-break would keep — deciding that here, using slot
/// order, would silently drop the wrong one. Retaining every tied member
/// instead defers the decision to the caller (`search_scan`'s caller,
/// `Db::search_vector`), which re-sorts by `(score desc, NodeId asc)` only
/// AFTER resolving every surviving slot to its `NodeId`, and truncates to
/// `k` only then. Evicting a tied group WHOLESALE (rather than one member at
/// a time) is what keeps the heap tightly bounded at `k` in the common case
/// (no ties) while staying exactly this conservative when there are ties:
/// once enough strictly-better elements have arrived to make an old
/// boundary tie provably irrelevant, the WHOLE group drops in one step —
/// never a slot-order-chosen subset of it.
pub(crate) fn push_topk(
    heap: &mut BinaryHeap<Reverse<(OrderedScore, u64)>>,
    score: f32,
    slot: u64,
    k: usize,
) {
    if k == 0 {
        return;
    }
    heap.push(Reverse((OrderedScore(score), slot)));
    while heap.len() > k {
        // heap.len() > k >= 1 => heap.len() >= 1, so the peek below is on a
        // non-empty heap.
        let Reverse((min_score, _)) = *heap.peek().expect("heap.len() > k >= 1: non-empty");
        // Drain every element tied at the current minimum score into `group`.
        let mut group = Vec::new();
        while let Some(&Reverse((next_score, _))) = heap.peek() {
            if next_score != min_score {
                break;
            }
            group.push(heap.pop().expect("just peeked"));
        }
        if heap.len() >= k {
            // Safe: removing the WHOLE tied group still leaves >= k
            // strictly-better elements — none of `group` can be in the
            // true top-k regardless of `NodeId` order. Drop it.
        } else {
            // Unsafe: some (but we can't tell which) of `group` is needed to
            // fill out `k` — restore all of it and stop shrinking.
            heap.extend(group);
            break;
        }
    }
}

/// Slot + score hits for `q`, ranked score-desc with ties at the k-boundary
/// retained conservatively (see [`push_topk`]) within each requested scope's
/// own `(model, scope)` cluster of the v4 `vectors` table, merged across
/// scopes. Does **not** apply the final `(score desc, NodeId asc)`
/// sort/truncate — `Db::search_vector` (`vector.rs`) does that after
/// resolving every returned slot to a `NodeId` (the tie-break seam: slot
/// order is not ULID order).
///
/// - An unknown `model` (never interned) yields `Ok(vec![])` — no error.
/// - Every scored row is routed through `cosine`, so a zero-norm row (either
///   side) is skipped exactly as the old RAM slab skipped it.
/// - A row whose stored vector length doesn't match `q.vector`'s is
///   skipped, not rejected — mirrors the old per-slab `slab.dim !=
///   q.vector.len()` skip. (The task brief's interface sketch describes this
///   as a `vector_dims` mismatch → `Rejected`; that would break
///   `tests/vector_search.rs::fully_tombstoned_model_still_rejects_a_new_dimension`
///   and `tests/differential.rs`'s explicit dim-mismatch probe, both of
///   which require an EMPTY result for a query vector whose length disagrees
///   with the model's pinned dim, matching the old engine and the
///   differential reference model's per-embedding skip. Implemented as a
///   skip to match; see the Task 5 report.)
/// - `q.candidates`, when set, restricts scoring to those `NodeId`s (deduped,
///   like the old RAM-slab filter) via `read_qvec_by_slot`'s O(1)
///   per-candidate lookup rather than a range scan — the candidates fast
///   path.
///
/// **F8 Task 4 routing.** `hnsw_meta`/`hnsw_links` are the `HNSW_META`/
/// `HNSW_LINKS` tables, opened by the caller (`vector.rs`) from the SAME read
/// transaction as everything else here. Only the non-candidates (whole-scope)
/// loop routes per scope: `hnsw::read_meta` tells whether that `(model,
/// scope)` cluster is built; if so, `hnsw::search` walks the graph instead of
/// a range scan, feeding the exact same `push_topk` heap. Otherwise the
/// pre-Task-4 range-scan body runs UNCHANGED. The candidates path never
/// consults the graph — a candidate list is already small and pre-resolved,
/// so a graph walk would buy nothing. `debug_used_graph` is debug-only
/// instrumentation (never replay state): set to `true`/`false` by whichever
/// branch a given scope takes, overwritten on every scope iteration, so it
/// reflects only the LAST scope processed — see
/// `Db::debug_last_search_used_graph`.
pub(crate) fn search_scan(
    tx: &ReadTransaction,
    dicts: &Dicts,
    scope_registry: &ScopeRegistry,
    q: &VectorQuery,
    hnsw_meta: &impl ReadableTable<&'static [u8], &'static [u8]>,
    hnsw_links: &impl ReadableTable<&'static [u8], &'static [u8]>,
    debug_used_graph: &AtomicBool,
) -> Result<Vec<(u64, f32)>, TopoError> {
    let Some(model_id) = dicts.id_of(DictKind::Model, &q.model) else {
        return Ok(Vec::new());
    };

    let vectors = tx.open_table(VECTORS).map_err(storage_err)?;
    let refs = tx.open_table(EMBEDDING_REF).map_err(storage_err)?;

    // `model_id`'s pinned dim (see `storage::check_or_pin_dim`), read once up
    // front so the graph branch below can skip a whole cluster when the
    // query's dim doesn't match — mirroring the scan branch's per-vector
    // `vector.len() != q.vector.len()` skip, which `hnsw::search` cannot do
    // internally since `cosine` silently zip-truncates mismatched lengths
    // instead of erroring. `None` (model never embedded) can't match any
    // built cluster either, so the graph branch treats it the same as a dim
    // mismatch.
    let pinned_dim: Option<u32> = {
        let dims = tx
            .open_table(crate::storage::VECTOR_DIMS)
            .map_err(storage_err)?;
        match dims
            .get(model_id.to_be_bytes().as_slice())
            .map_err(storage_err)?
        {
            Some(v) => {
                let bytes: [u8; 4] = v
                    .value()
                    .try_into()
                    .map_err(|_| TopoError::Encoding("bad vector_dims value".into()))?;
                Some(u32::from_le_bytes(bytes))
            }
            None => None,
        }
    };

    let mut heap: BinaryHeap<Reverse<(OrderedScore, u64)>> = BinaryHeap::new();

    // Quantize the query once. A zero-norm query codes to all-zero, which
    // `cosine_q` would score `None` against every row — same as the old
    // `cosine` behavior for a zero-norm query — so short-circuit to `[]`.
    let (_, qcodes) = crate::quant::quantize(&q.vector);
    if crate::quant::is_zero(&qcodes) {
        return Ok(Vec::new());
    }

    if let Some(candidates) = &q.candidates {
        let node_slots = tx.open_table(NODE_SLOTS).map_err(storage_err)?;
        let allowed_scopes: HashSet<u32> = q
            .scopes
            .iter_scopes()
            .filter_map(|s| scope_registry.id_of(s))
            .collect();
        // Dedup, like the old RAM-slab filter (`HashSet<NodeId>`) — a
        // repeated id in `candidates` must not double-score its row.
        let distinct: HashSet<_> = candidates.iter().copied().collect();
        for id in distinct {
            let Some(slot) = node_slot(&node_slots, id)? else {
                continue;
            };
            let Some((row_model, row_scope, _scale, codes)) =
                read_qvec_by_slot(&vectors, &refs, slot)?
            else {
                continue;
            };
            if row_model != model_id || !allowed_scopes.contains(&row_scope) {
                continue;
            }
            if codes.len() != q.vector.len() {
                continue;
            }
            if let Some(score) = crate::quant::cosine_q(&qcodes, &codes) {
                push_topk(&mut heap, score, slot, q.k);
            }
        }
    } else {
        for scope in q.scopes.iter_scopes() {
            let Some(scope_id) = scope_registry.id_of(scope) else {
                continue;
            };
            let built_meta = hnsw::read_meta(hnsw_meta, model_id, scope_id)?.filter(|m| {
                // Format headroom, not a real optionality: `build_cluster`
                // REMOVES a cluster's `HNSW_META` row entirely rather than
                // ever writing `built: false`, so every row this read can
                // observe already has `built == true`. Asserted rather than
                // trusted so a future writer that starts persisting
                // half-built rows trips here instead of silently reaching
                // the graph branch below with an unbuilt cluster.
                debug_assert!(m.built, "HNSW_META row present with built == false");
                m.built
            });
            let dim_matches = pinned_dim == Some(q.vector.len() as u32);
            let was_built = built_meta.is_some();
            if let Some(meta_row) = built_meta.filter(|_| dim_matches) {
                debug_used_graph.store(true, Ordering::SeqCst);
                let reader = hnsw::GraphReader {
                    vectors: &vectors,
                    refs: &refs,
                    model: model_id,
                    scope: scope_id,
                };
                let ef = hnsw::ef_search(q.k);
                let hits = hnsw::search(hnsw_links, &meta_row, &reader, &q.vector, ef, q.k)?;
                for (slot, score) in hits {
                    push_topk(&mut heap, score, slot, q.k);
                }
            } else if was_built {
                // Built cluster but the query's dim doesn't match the
                // cluster's pinned dim (see `pinned_dim` above) — same `[]`
                // semantics as the scan branch's per-vector dim skip below,
                // just applied to the whole cluster since the graph has no
                // per-vector fallback. `debug_used_graph` is left at its
                // prior value: no work happened for this scope, so neither
                // "used graph" nor "used scan" applies to it.
            } else {
                debug_used_graph.store(false, Ordering::SeqCst);
                let prefix = vector_prefix(model_id, scope_id);
                let mut start = prefix.to_vec();
                start.extend_from_slice(&0u64.to_be_bytes());
                let mut end = prefix.to_vec();
                end.extend_from_slice(&u64::MAX.to_be_bytes());
                for entry in vectors
                    .range(start.as_slice()..=end.as_slice())
                    .map_err(storage_err)?
                {
                    let (key_guard, value_guard) = entry.map_err(storage_err)?;
                    let key = key_guard.value();
                    let slot_bytes: [u8; 8] = key[8..16]
                        .try_into()
                        .map_err(|_| TopoError::Encoding("bad vector_key length".into()))?;
                    let slot = u64::from_be_bytes(slot_bytes);
                    let raw = unframe_value(value_guard.value())?;
                    let (_scale, codes): (f32, Vec<i8>) = postcard::from_bytes(&raw)
                        .map_err(|e| TopoError::Encoding(e.to_string()))?;
                    if codes.len() != q.vector.len() {
                        continue;
                    }
                    if let Some(score) = crate::quant::cosine_q(&qcodes, &codes) {
                        push_topk(&mut heap, score, slot, q.k);
                    }
                }
            }
        }
    }

    Ok(heap
        .into_iter()
        .map(|Reverse((score, slot))| (slot, score.0))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use redb::Database;

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        (dir, db)
    }

    /// Bounded prefix scan over exactly one `(model, scope)` cluster —
    /// standalone here (rather than reusing `read_qvec_by_slot`) because
    /// the whole point is to prove NO ORPHAN rows exist anywhere in that
    /// range, not just that one particular slot resolves correctly.
    fn cluster_rows(
        vectors: &impl ReadableTable<&'static [u8], &'static [u8]>,
        model: u32,
        scope: u32,
    ) -> Vec<[u8; 16]> {
        let prefix = vector_prefix(model, scope);
        let mut start = prefix.to_vec();
        start.extend_from_slice(&0u64.to_be_bytes());
        let mut end = prefix.to_vec();
        end.extend_from_slice(&u64::MAX.to_be_bytes());
        vectors
            .range(start.as_slice()..=end.as_slice())
            .unwrap()
            .map(|entry| {
                let (k, _) = entry.unwrap();
                k.value().try_into().unwrap()
            })
            .collect()
    }

    #[test]
    fn put_read_round_trips() {
        let (_dir, db) = open();
        let v = vec![0.5f32, -2.0, 1.0];
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut refs = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vectors, &mut refs, 1, 2, 7, &v).unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        let (model, scope, scale, codes) = read_qvec_by_slot(&vectors, &refs, 7)
            .unwrap()
            .expect("row present");
        assert_eq!((model, scope), (1, 2));
        assert_eq!(scale, 2.0);
        assert_eq!(codes, vec![32i8, -127, 64]);
        // `dequantize` recovers the approximation of v from the stored
        // (scale, codes) pair.
        let approx = crate::quant::dequantize(scale, &codes);
        for (a, b) in approx.iter().zip(&v) {
            assert!((a - b).abs() <= b.abs() * 0.01 + 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn reembed_same_model_overwrites_in_place_no_orphan() {
        let (_dir, db) = open();
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut refs = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vectors, &mut refs, 1, 2, 7, &[1.0, 2.0]).unwrap();
            put_vector(&mut vectors, &mut refs, 1, 2, 7, &[9.0, 9.0]).unwrap();

            let rows = cluster_rows(&vectors, 1, 2);
            assert_eq!(
                rows.len(),
                1,
                "same-model re-embed must not leave an orphan row"
            );
            assert_eq!(rows[0], vector_key(1, 2, 7));
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        let (model, scope, scale, codes) = read_qvec_by_slot(&vectors, &refs, 7).unwrap().unwrap();
        assert_eq!((model, scope), (1, 2));
        assert_eq!(scale, 9.0);
        assert_eq!(codes, vec![127i8, 127]); // round(9.0*127.0/9.0)=127 for both
    }

    #[test]
    fn reembed_under_new_model_deletes_old_models_row_and_updates_ref() {
        let (_dir, db) = open();
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut refs = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vectors, &mut refs, 1, 2, 7, &[1.0, 2.0]).unwrap();
            // Re-embed the SAME slot under model 5 instead of model 1.
            put_vector(&mut vectors, &mut refs, 5, 2, 7, &[3.0, 4.0]).unwrap();

            // Old model's (model=1, scope=2) range is now empty.
            assert!(
                cluster_rows(&vectors, 1, 2).is_empty(),
                "old model's cluster must be empty after a cross-model re-embed"
            );
            // New model's row is present.
            let rows = cluster_rows(&vectors, 5, 2);
            assert_eq!(rows, vec![vector_key(5, 2, 7)]);
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        let (model, scope, scale, codes) = read_qvec_by_slot(&vectors, &refs, 7).unwrap().unwrap();
        assert_eq!((model, scope), (5, 2));
        assert_eq!(scale, 4.0);
        // s = 127/4.0 = 31.75; round(3.0*31.75)=round(95.25)=95; round(4.0*31.75)=127
        assert_eq!(codes, vec![95i8, 127]);
    }

    #[test]
    fn remove_vector_clears_both_tables() {
        let (_dir, db) = open();
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut refs = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vectors, &mut refs, 1, 2, 7, &[1.0, 2.0]).unwrap();
            remove_vector(&mut vectors, &mut refs, 7).unwrap();
            assert!(cluster_rows(&vectors, 1, 2).is_empty());
            assert!(refs.get(slot_key(7).as_slice()).unwrap().is_none());
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        assert!(read_qvec_by_slot(&vectors, &refs, 7).unwrap().is_none());

        // Also a no-op (not an error) on a slot with no ref at all.
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut refs = tx.open_table(EMBEDDING_REF).unwrap();
            remove_vector(&mut vectors, &mut refs, 999).unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn read_qvec_by_slot_never_embedded_is_none() {
        let (_dir, db) = open();
        let tx = db.begin_write().unwrap();
        {
            tx.open_table(VECTORS).unwrap();
            tx.open_table(EMBEDDING_REF).unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        assert!(read_qvec_by_slot(&vectors, &refs, 42).unwrap().is_none());
    }

    // -- Task 5 Step 1: streaming-heap top-k ≡ sort-and-truncate -----------

    /// Sorts `entries` by `(score desc via total_cmp, slot asc)` — the same
    /// order `push_topk`'s heap uses internally — and truncates to `k`. The
    /// shared helper both the proptest and the targeted test below compare
    /// the heap's output against.
    fn sort_and_truncate(entries: &[(f32, u64)], k: usize) -> Vec<(f32, u64)> {
        let mut out = entries.to_vec();
        out.sort_by(|a, b| {
            OrderedScore(b.0)
                .cmp(&OrderedScore(a.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        out.truncate(k);
        out
    }

    fn heap_topk(entries: &[(f32, u64)], k: usize) -> Vec<(f32, u64)> {
        let mut heap: BinaryHeap<Reverse<(OrderedScore, u64)>> = BinaryHeap::new();
        for &(score, slot) in entries {
            push_topk(&mut heap, score, slot, k);
        }
        // `push_topk` conservatively over-retains ties at the boundary (see
        // its doc) — apply the SAME final sort+truncate a caller would, so
        // this is comparable to `sort_and_truncate`'s plain top-k.
        let raw: Vec<(f32, u64)> = heap
            .into_iter()
            .map(|Reverse((score, slot))| (score.0, slot))
            .collect();
        sort_and_truncate(&raw, k)
    }

    /// Explicit, non-random case: three candidates tied at the score that
    /// falls exactly on the k=2 boundary. A heap that evicts on ANY push past
    /// `k` (breaking ties via slot/insertion order) would arbitrarily keep
    /// only 2 of the 3 — but which 2 is exactly the decision `push_topk` must
    /// NOT make, since the eventual winner is decided by `NodeId` order
    /// upstream (`Db::search_vector`), not by slot. All 3 must survive the
    /// heap so the caller's later NodeId-order sort can pick correctly.
    #[test]
    fn heap_retains_all_ties_at_the_boundary_conservatively() {
        let mut heap: BinaryHeap<Reverse<(OrderedScore, u64)>> = BinaryHeap::new();
        for slot in [10u64, 5, 20] {
            push_topk(&mut heap, 1.0, slot, 2);
        }
        let mut slots: Vec<u64> = heap.into_iter().map(|Reverse((_, slot))| slot).collect();
        slots.sort_unstable();
        assert_eq!(
            slots,
            vec![5, 10, 20],
            "all boundary ties must be retained, not just k"
        );
    }

    /// A strictly-better element arriving later must still be able to push a
    /// whole tied-at-the-old-boundary group out once there are enough
    /// strictly-better elements to make the tie irrelevant.
    #[test]
    fn heap_drops_ties_once_enough_strictly_better_elements_arrive() {
        let mut heap: BinaryHeap<Reverse<(OrderedScore, u64)>> = BinaryHeap::new();
        for slot in [1u64, 2, 3] {
            push_topk(&mut heap, 1.0, slot, 1); // three-way tie, k=1
        }
        push_topk(&mut heap, 5.0, 4, 1); // strictly better — the tie is now moot
        let slots: Vec<u64> = heap.into_iter().map(|Reverse((_, slot))| slot).collect();
        assert_eq!(
            slots,
            vec![4],
            "a strictly-better element must fully displace a moot tie"
        );
    }

    proptest! {
        #[test]
        fn streaming_heap_topk_matches_sort_and_truncate(
            entries in proptest::collection::vec((-1000.0f32..1000.0f32, 0u64..10_000u64), 0..200),
            k in 1usize..20,
        ) {
            prop_assert_eq!(heap_topk(&entries, k), sort_and_truncate(&entries, k));
        }
    }
}
