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
use crate::scopes::ScopeRegistry;
use crate::slots::{node_slot, NODE_SLOTS};
use crate::vector::VectorQuery;
use redb::{ReadTransaction, ReadableTable, Table, TableDefinition};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

/// Clustered embedding rows: `vector_key(model, scope, slot)` -> framed
/// postcard `Vec<f32>`.
pub(crate) const VECTORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("vectors");
/// Per-node pointer to its current `(model, scope)`: 8-byte BE node slot ->
/// postcard `(u32, u32)`. Small fixed-size value — not framed; framing exists
/// to lz4-compress large payloads and a `(u32, u32)` never crosses that
/// threshold. Lets `put_vector`/`remove_vector`/`read_vector_by_slot` find a
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

fn decode_ref(bytes: &[u8]) -> Result<(u32, u32), TopoError> {
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
        }
    }
    let raw = postcard::to_allocvec(v).map_err(|e| TopoError::Encoding(e.to_string()))?;
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
pub(crate) fn read_vector_by_slot(
    vectors: &impl ReadableTable<&'static [u8], &'static [u8]>,
    refs: &impl ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Option<(u32, u32, Vec<f32>)>, TopoError> {
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
            let vec: Vec<f32> =
                postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some((model, scope, vec)))
        }
        None => Err(TopoError::Encoding(
            "read_vector_by_slot: embedding_ref present but vectors row missing".into(),
        )),
    }
}

/// Bit-for-bit the same cosine formula the v3 RAM slab scored with
/// (`vector.rs`'s old `Slab::top_k`, relocated here for the Task 5 disk read
/// path): a single accumulation pass over `a.iter().zip(b)`, `None` when
/// either side's squared-norm accumulator is exactly `0.0` — the zero-norm
/// skip. Every scored row in [`search_scan`] is routed through this, never a
/// hand-rolled dot product.
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

/// A total order over `f32` scores via [`f32::total_cmp`] — `f32` isn't
/// `Ord`, so the heap wraps every score in this newtype.
///
/// **NaN finding:** the write path (`storage.rs::apply_op`'s `SetEmbedding`
/// arm) never validates finiteness of an embedding's components, so a NaN or
/// ±Infinity cosine score IS reachable today, exactly as it was under the
/// old RAM slab. The old merge sort handled this via
/// `partial_cmp(..).unwrap_or(Equal)`, which silently treats any NaN
/// comparison as "equal" — not a panic, but not a well-defined order either.
/// `total_cmp` replicates that "NaN doesn't crash the sort" behavior while
/// additionally giving it a deterministic total position, which is a strict
/// improvement, not new validation: a non-finite score still ranks (it is
/// never rejected). No test exercises a NaN score on either the old or new
/// path.
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
///   like the old RAM-slab filter) via `read_vector_by_slot`'s O(1)
///   per-candidate lookup rather than a range scan — the candidates fast
///   path.
pub(crate) fn search_scan(
    tx: &ReadTransaction,
    dicts: &Dicts,
    scope_registry: &ScopeRegistry,
    q: &VectorQuery,
) -> Result<Vec<(u64, f32)>, TopoError> {
    let Some(model_id) = dicts.id_of(DictKind::Model, &q.model) else {
        return Ok(Vec::new());
    };

    let vectors = tx.open_table(VECTORS).map_err(storage_err)?;
    let refs = tx.open_table(EMBEDDING_REF).map_err(storage_err)?;

    let mut heap: BinaryHeap<Reverse<(OrderedScore, u64)>> = BinaryHeap::new();

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
            let Some((row_model, row_scope, vector)) = read_vector_by_slot(&vectors, &refs, slot)?
            else {
                continue;
            };
            if row_model != model_id || !allowed_scopes.contains(&row_scope) {
                continue;
            }
            if vector.len() != q.vector.len() {
                continue;
            }
            if let Some(score) = cosine(&vector, &q.vector) {
                push_topk(&mut heap, score, slot, q.k);
            }
        }
    } else {
        for scope in q.scopes.iter_scopes() {
            let Some(scope_id) = scope_registry.id_of(scope) else {
                continue;
            };
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
                let vector: Vec<f32> =
                    postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
                if vector.len() != q.vector.len() {
                    continue;
                }
                if let Some(score) = cosine(&vector, &q.vector) {
                    push_topk(&mut heap, score, slot, q.k);
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
    /// standalone here (rather than reusing `read_vector_by_slot`) because
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
        let tx = db.begin_write().unwrap();
        {
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut refs = tx.open_table(EMBEDDING_REF).unwrap();
            put_vector(&mut vectors, &mut refs, 1, 2, 7, &[1.0, 2.0, 3.0]).unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        let (model, scope, vector) = read_vector_by_slot(&vectors, &refs, 7).unwrap().unwrap();
        assert_eq!((model, scope), (1, 2));
        assert_eq!(vector, vec![1.0, 2.0, 3.0]);
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
        let (model, scope, vector) = read_vector_by_slot(&vectors, &refs, 7).unwrap().unwrap();
        assert_eq!((model, scope), (1, 2));
        assert_eq!(vector, vec![9.0, 9.0]);
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
        let (model, scope, vector) = read_vector_by_slot(&vectors, &refs, 7).unwrap().unwrap();
        assert_eq!((model, scope), (5, 2));
        assert_eq!(vector, vec![3.0, 4.0]);
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
        assert!(read_vector_by_slot(&vectors, &refs, 7).unwrap().is_none());

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
    fn read_vector_by_slot_never_embedded_is_none() {
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
        assert!(read_vector_by_slot(&vectors, &refs, 42).unwrap().is_none());
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
