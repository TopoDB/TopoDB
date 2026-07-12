//! v4 clustered vector storage (Task 3): `vectors` keyed by `(model, scope,
//! slot)` — a fixed-width, BE-sortable key so a single model+scope's rows
//! are one contiguous, boundedly-scannable range — plus `embedding_ref`, a
//! slot-keyed pointer to a node's CURRENT `(model, scope)` so a re-embed or
//! removal can find (and delete) the OLD `vectors` row in O(1) rather than
//! scanning.
//!
//! Dual-written alongside the still-authoritative v3 `EMBEDDINGS` table
//! (`storage.rs`) by `apply_op`'s `SetEmbedding`/`RemoveNode` arms. Nothing
//! reads these two tables on any query path yet — the in-RAM slab index
//! (`vector.rs`) keeps serving `search_vector` until a later v4 task cuts
//! over the read path.
use crate::codec::{frame_value, unframe_value};
use crate::error::{storage_err, TopoError};
use redb::{ReadableTable, Table, TableDefinition};

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
/// model+scope's rows without touching any other cluster. No production
/// caller yet — this is the read path a later v4 task (the search cutover
/// off the RAM slab) will use for cluster-local scans; exercised for now by
/// this module's own no-orphan tests, hence the `#[allow(dead_code)]`
/// (mirrors `Storage::open`'s identical rationale in storage.rs).
#[allow(dead_code)]
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
/// ULID-mapping-without-record-row two-cause miss split. No production
/// caller yet (see `vector_prefix`'s identical note) — exercised by this
/// module's tests and by `storage.rs`'s Task-3 consistency cross-check test.
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
