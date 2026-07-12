//! v3 -> v4 migration: folds the old slot-keyed `embeddings` table into the
//! v4 clustered `vectors`/`embedding_ref` tables (`vector_store.rs`), and —
//! when the caller says the postings table still carries the pre-Task-6
//! single-row-per-term layout — re-chunks POSTINGS into the v4 chunked
//! block layout (`fts.rs`'s `chunked_posting_key`/`encode_posting_block`).
//! The caller (`Storage::open_with_options`'s version-match arm) deletes the
//! old `embeddings` table itself, once this function's vectors pass has
//! drained everything worth keeping out of it — `redb::WriteTransaction::
//! delete_table` requires no live `Table` guard for that table, and this
//! function only ever borrows it read-only, so the caller's own guard is the
//! only one that needs dropping first.
//!
//! ## Postings key-shape discrimination
//!
//! A v3-native POSTINGS row is keyed `scope_id.to_be_bytes() ++ term` (no
//! trailing chunk suffix — the pre-Task-6 `posting_key`, deleted from
//! `fts.rs` by that task). A v4 chunked row is keyed `scope_id.to_be_bytes()
//! ++ term ++ chunk.to_be_bytes()`. These two key shapes are NOT reliably
//! distinguishable by inspecting raw bytes alone: a v3 row for a term ending
//! in 4 bytes that happen to look like a chunk-index suffix is
//! byte-indistinguishable from a chunked row for a term 4 bytes shorter.
//! Resolving that requires knowing, out of band, which encoding scheme
//! wrote a given table — so this module does not attempt content-sniffing.
//! Instead the caller states it explicitly via `postings_already_chunked`,
//! derived from which `Storage::open_with_options` match arm is calling:
//!
//! - `Some(3)` (a file whose LAST write was genuine pre-Task-7 v3 code, or
//!   `migrate_v3::migrate_v2_to_v3`'s FTS rebuild running under the OLD
//!   `fts_update` before Task 6's chunking wiring landed — both write only
//!   single-row postings, never chunked ones) → `false`: the postings pass
//!   below runs and re-chunks every row.
//! - `Some(2)` / `Some(1)` chains (this function called AFTER
//!   `migrate_v3::migrate_v2_to_v3` has already run in the SAME open, using
//!   THIS build's `fts_update`, which calls `fts.rs`'s current — chunked —
//!   `set_posting` exclusively) → `true`: POSTINGS already holds only
//!   chunked rows: the postings pass is skipped entirely.
//!
//! The "a version-3 file holds only single-row postings" premise behind the
//! `Some(3)` arm is a PROCESS/RELEASE invariant, not a code-enforced one: no
//! RELEASED build ever writes chunked postings under `format_version == 3`,
//! but this branch's own history refutes the stronger claim — commits
//! 9b3d5a7 through 70bcd09 (Task 6, "format flip pending") stamped 3 while
//! writing CHUNKED postings, so transitional mid-branch files with exactly
//! that mixed provenance did exist (test-only; regenerated rather than
//! migrated). Should such a file — or any future chunked-under-3 state —
//! reach the `Some(3)` arm anyway, the load-bearing backstop is
//! `decode_v3_posting_value`'s trailing-bytes check: a chunked block's
//! leading `POSTINGS_BLOCK_FORMAT_V0` (0x00) format byte decodes as varint
//! `count == 0`, the entry loop reads nothing, and the block's remaining
//! bytes (non-empty for any stored chunk — empty chunks are never written)
//! trip the trailing-bytes rejection. The migration therefore fails LOUDLY
//! with `TopoError::Encoding` (aborting the whole open; the file is left
//! byte-intact, since the write transaction never commits) instead of
//! silently corrupting the table. That behavior is byte-layout-coincidental
//! — a future block-codec change (e.g. a nonzero format tag that happens to
//! decode as a plausible count) could silently flip it to corruption — so
//! it is PINNED by
//! `chunked_postings_under_version_3_fail_migration_loudly_not_silently`
//! (`storage.rs` tests): any codec change that breaks the backstop breaks
//! that test. If the calling arm's provenance ever becomes genuinely
//! ambiguous, this function must not guess; the arm should refuse to
//! migrate rather than rely on the backstop alone.
//!
//! ## Idempotency
//!
//! `Some(2)`/`Some(1)` chains reach this function AFTER
//! `migrate_v3::migrate_v2_to_v3` has already dual-written `vectors`/
//! `embedding_ref` from the very same (by-then slot-keyed) `embeddings` rows
//! this function's vectors pass also scans. Rather than detect and skip
//! already-present rows, the vectors pass below runs UNCONDITIONALLY and
//! overwrites: `check_or_pin_dim` seeing the same `(model, dim)` twice is
//! `Ok(())` both times, and `vector_store::put_vector` writing the identical
//! `(model, scope, slot, vector)` a second time produces a byte-identical
//! row (same key, same re-encoded value) — a harmless redundant write, not a
//! correctness hazard, and simpler than threading a "was this already
//! written by migrate_v2_to_v3" flag through the call chain.
use crate::codec::unframe_value;
use crate::dict::{DictKind, Dicts};
use crate::error::{storage_err, TopoError};
use crate::storage::{check_or_pin_dim, slot_key};
use crate::vector_store::put_vector;
use redb::{ReadableTable, Table};
use std::collections::HashMap;

/// Frozen decoder for the pre-Task-6 v3 single-row-per-term POSTINGS value:
/// `[count varint]` then per entry, ascending by slot, `[slot_delta
/// varint][tf varint]` — critically, NO leading format-tag byte (unlike the
/// new `POSTINGS_BLOCK_FORMAT_V0`-tagged chunked block format `fts.rs` now
/// writes exclusively). This exact shape was deleted from `fts.rs` by Task 6
/// (`decode_postings`) — resurrected here, `migrate_v3.rs`-style, because a
/// genuine pre-v4 v3 file on disk still carries it and this migration has to
/// be able to read it.
fn decode_v3_posting_value(payload: &[u8]) -> Result<Vec<(u64, u32)>, TopoError> {
    let mut input = payload;
    let count = usize::try_from(crate::adj::read_varint(&mut input)?)
        .map_err(|_| TopoError::Encoding("v3 postings count too large".into()))?;
    let mut entries = Vec::with_capacity(count);
    let mut slot = 0u64;
    for _ in 0..count {
        slot = slot
            .checked_add(crate::adj::read_varint(&mut input)?)
            .ok_or_else(|| TopoError::Encoding("v3 postings slot overflow".into()))?;
        let tf = u32::try_from(crate::adj::read_varint(&mut input)?)
            .map_err(|_| TopoError::Encoding("v3 postings tf too large".into()))?;
        entries.push((slot, tf));
    }
    if !input.is_empty() {
        return Err(TopoError::Encoding(
            "trailing bytes in v3 postings value".into(),
        ));
    }
    Ok(entries)
}

/// Frozen decoder for the pre-Task-6 v3 POSTINGS key: `scope_id.to_be_bytes()
/// ++ term-UTF-8`, no chunk suffix (mirrors the deleted `fts::posting_key`).
fn decode_v3_posting_key(key: &[u8]) -> Result<(u32, String), TopoError> {
    if key.len() < 4 {
        return Err(TopoError::Encoding("v3 postings key too short".into()));
    }
    let scope_id = u32::from_be_bytes(key[0..4].try_into().expect("checked len >= 4"));
    let term = std::str::from_utf8(&key[4..])
        .map_err(|e| TopoError::Encoding(format!("v3 postings key not valid utf8: {e}")))?
        .to_string();
    Ok((scope_id, term))
}

/// Migrates a v3-format `Storage` in place, within the caller's already-open
/// write transaction: folds `embeddings` (slot-keyed, joined against `nodes`
/// for each row's scope) into `vectors`/`embedding_ref`, pinning/checking
/// each model's dim exactly like the live `SetEmbedding` write path — a v3
/// file with one model recorded at two different dims across scopes (legal
/// v3 state, since the old RAM slab pinned dims per-`(model, scope)`, not
/// per-model) now hits the v4 per-model-only policy and fails the WHOLE
/// migration with `TopoError::Rejected`, naming the model and both dims —
/// not `Encoding`: this is legal upstream data meeting a new v4 policy, not
/// file corruption. See the module doc comment for the postings-pass
/// discrimination (`postings_already_chunked`) and the vectors-pass
/// idempotency rationale.
///
/// Does NOT delete `embeddings` or touch FTS_DOCS/FTS_STATS (unchanged
/// across v3->v4 — only the POSTINGS key/value shape changes) or stamp
/// `format_version` — all three are the caller's responsibility, once this
/// function's borrows of `embeddings`/`nodes` have ended.
#[allow(clippy::too_many_arguments)]
pub(crate) fn migrate_v3_to_v4(
    embeddings: &impl ReadableTable<&'static [u8], &'static [u8]>,
    nodes: &impl ReadableTable<&'static [u8], &'static [u8]>,
    vector_dims: &mut Table<'_, &'static [u8], &'static [u8]>,
    vectors: &mut Table<'_, &'static [u8], &'static [u8]>,
    embedding_ref: &mut Table<'_, &'static [u8], &'static [u8]>,
    dict_table: &mut Table<'_, &'static [u8], &'static str>,
    dicts: &mut Dicts,
    postings: &mut Table<'_, &'static [u8], &'static [u8]>,
    postings_already_chunked: bool,
) -> Result<(), TopoError> {
    // -- vectors pass: embeddings (slot-keyed) -> vectors/embedding_ref ----
    for entry in embeddings.iter().map_err(storage_err)? {
        let (k, v) = entry.map_err(storage_err)?;
        let key: [u8; 8] = k
            .value()
            .try_into()
            .map_err(|_| TopoError::Encoding("bad embedding slot key".into()))?;
        let slot = u64::from_be_bytes(key);
        let raw = unframe_value(v.value())?;
        let (model, vector): (String, Vec<f32>) =
            postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;

        let node_v = nodes
            .get(slot_key(slot).as_slice())
            .map_err(storage_err)?
            .ok_or_else(|| {
                TopoError::Encoding(format!(
                    "migrate_v3_to_v4: embeddings row at slot {slot} has no matching NODES row"
                ))
            })?;
        let raw_node = unframe_value(node_v.value())?;
        let disk: crate::disk::NodeRecordDiskV3 = postcard::from_bytes(raw_node.as_ref())
            .map_err(|e| TopoError::Encoding(e.to_string()))?;
        let scope_id = disk.scope;
        drop(node_v);

        let model_id = dicts.intern(dict_table, DictKind::Model, &model)?;
        check_or_pin_dim(vector_dims, model_id, vector.len()).map_err(|e| match e {
            TopoError::Rejected(msg) => {
                TopoError::Rejected(format!("migrating v3 embedding for model {model:?}: {msg}"))
            }
            other => other,
        })?;
        put_vector(vectors, embedding_ref, model_id, scope_id, slot, &vector)?;
    }

    // -- postings pass: single-row v3 rows -> chunked v4 blocks -----------
    if !postings_already_chunked {
        let mut grouped: HashMap<(u32, String), Vec<(u64, u32)>> = HashMap::new();
        let mut old_keys: Vec<Vec<u8>> = Vec::new();
        for entry in postings.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let key_bytes = k.value().to_vec();
            let (scope_id, term) = decode_v3_posting_key(&key_bytes)?;
            let raw = unframe_value(v.value())?;
            let decoded = decode_v3_posting_value(raw.as_ref())?;
            grouped.entry((scope_id, term)).or_default().extend(decoded);
            old_keys.push(key_bytes);
        }
        // Old rows removed BEFORE any chunked row is written: a v3 key (no
        // chunk suffix, length `4 + term.len()`) can never collide with a
        // chunked key (length `4 + term.len() + 4`) for the same term, so
        // this ordering is not load-bearing for correctness — it just keeps
        // the table from ever holding "old row + its already-migrated
        // chunked replacement" at the same time mid-pass, for a cleaner
        // storage_report during migration.
        for key in &old_keys {
            postings.remove(key.as_slice()).map_err(storage_err)?;
        }
        for ((scope_id, term), mut entries) in grouped {
            // Old rows were already stored ascending-by-slot (frozen v3
            // decode preserves that), but entries from the SAME term can
            // only ever have come from one old row (v3 never split a term
            // across rows), so this sort is a no-op in practice — kept as
            // an explicit invariant rather than an assumption.
            entries.sort_by_key(|&(slot, _)| slot);
            for (slot, tf) in entries {
                // Ascending-slot replay through the exact same incremental,
                // tested chunk-splitting logic the live write path uses —
                // see `fts::set_posting`'s doc comment and this module's.
                crate::fts::set_posting(postings, scope_id, &term, slot, tf)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adj::write_varint;
    use crate::codec::frame_value;
    use crate::dict::DICT;
    use crate::disk::node_to_disk_v3;
    use crate::fts::{chunked_posting_key, posting_df, read_posting};
    use crate::ids::{NodeId, Scope};
    use crate::scopes::{seed_shared, ScopeRegistry, SCOPES};
    use crate::slots::{alloc_node_slot, node_slot, NODE_IDS, NODE_SLOTS};
    use crate::state::NodeRecord;
    use crate::storage::{EMBEDDINGS, META, NODES, VECTOR_DIMS};
    use crate::vector_store::{read_vector_by_slot, EMBEDDING_REF, VECTORS};
    use redb::Database;

    /// Old (pre-Task-6) single-row postings encode, mirroring the deleted
    /// `fts::encode_postings` exactly (count varint, then ascending
    /// slot-delta/tf pairs, NO format-tag byte) — used only by this test
    /// module to manufacture a v3-shaped POSTINGS row to migrate.
    fn encode_v3_posting_value(entries: &[(u64, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint(&mut out, entries.len() as u64);
        let mut previous = 0u64;
        for &(slot, tf) in entries {
            write_varint(&mut out, slot - previous);
            previous = slot;
            write_varint(&mut out, tf as u64);
        }
        out
    }

    fn v3_posting_key(scope_id: u32, term: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(4 + term.len());
        key.extend_from_slice(&scope_id.to_be_bytes());
        key.extend_from_slice(term.as_bytes());
        key
    }

    /// Shared test rig: an empty redb file with every table this migration
    /// touches created, and one node (`id`, `Scope::Shared`) already
    /// slotted/written into NODES so the vectors pass has something to join
    /// against. Returns `(_dir, db, slot, scope_id)`.
    fn rig(id: NodeId) -> (tempfile::TempDir, Database, u64, u32) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let (slot, scope_id) = {
            let tx = db.begin_write().unwrap();
            let scope_id;
            let slot;
            {
                let mut meta = tx.open_table(META).unwrap();
                let mut node_slots = tx.open_table(NODE_SLOTS).unwrap();
                let mut node_ids = tx.open_table(NODE_IDS).unwrap();
                alloc_node_slot(&mut meta, &mut node_slots, &mut node_ids, id).unwrap();
                slot = node_slot(&node_slots, id).unwrap().unwrap();

                let mut scopes_table = tx.open_table(SCOPES).unwrap();
                seed_shared(&mut scopes_table).unwrap();
                let mut scopes = ScopeRegistry::load_table_for_rebuild(&scopes_table).unwrap();
                let mut dict_table = tx.open_table(DICT).unwrap();
                let mut dicts = Dicts::default();
                let rec = NodeRecord {
                    id,
                    scope: Scope::Shared,
                    label: "M".into(),
                    props: Default::default(),
                    embedding: None,
                };
                let disk = node_to_disk_v3(
                    &rec,
                    &mut dict_table,
                    &mut dicts,
                    &mut scopes_table,
                    &mut scopes,
                )
                .unwrap();
                scope_id = disk.scope;
                let raw = postcard::to_allocvec(&disk).unwrap();
                let framed = frame_value(raw);
                let mut nodes = tx.open_table(NODES).unwrap();
                nodes
                    .insert(slot_key(slot).as_slice(), framed.as_slice())
                    .unwrap();
                tx.open_table(EMBEDDINGS).unwrap();
                tx.open_table(VECTOR_DIMS).unwrap();
                tx.open_table(VECTORS).unwrap();
                tx.open_table(EMBEDDING_REF).unwrap();
                tx.open_table(crate::storage::POSTINGS).unwrap();
            }
            tx.commit().unwrap();
            (slot, scope_id)
        };
        (dir, db, slot, scope_id)
    }

    #[test]
    fn vectors_pass_folds_embeddings_into_vectors_and_embedding_ref() {
        let id = NodeId::new();
        let (_dir, db, slot, scope_id) = rig(id);

        let tx = db.begin_write().unwrap();
        {
            let raw = postcard::to_allocvec(&("m1".to_string(), vec![1.0f32, 2.0, 3.0])).unwrap();
            let framed = frame_value(raw);
            let mut embeddings = tx.open_table(EMBEDDINGS).unwrap();
            embeddings
                .insert(slot_key(slot).as_slice(), framed.as_slice())
                .unwrap();
        }
        {
            let nodes = tx.open_table(NODES).unwrap();
            let embeddings = tx.open_table(EMBEDDINGS).unwrap();
            let mut vector_dims = tx.open_table(VECTOR_DIMS).unwrap();
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut embedding_ref = tx.open_table(EMBEDDING_REF).unwrap();
            let mut dict_table = tx.open_table(DICT).unwrap();
            let mut dicts = Dicts::default();
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            migrate_v3_to_v4(
                &embeddings,
                &nodes,
                &mut vector_dims,
                &mut vectors,
                &mut embedding_ref,
                &mut dict_table,
                &mut dicts,
                &mut postings,
                true,
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let vectors = tx.open_table(VECTORS).unwrap();
        let refs = tx.open_table(EMBEDDING_REF).unwrap();
        let (model_id, got_scope, vector) = read_vector_by_slot(&vectors, &refs, slot)
            .unwrap()
            .expect("vectors pass must populate the v4 tables");
        assert_eq!(got_scope, scope_id);
        assert_eq!(vector, vec![1.0, 2.0, 3.0]);
        let dict_table = tx.open_table(DICT).unwrap();
        let dicts = Dicts::load_from_table(&dict_table).unwrap();
        assert_eq!(dicts.resolve(DictKind::Model, model_id).unwrap(), "m1");
    }

    /// The controller-adjudicated error variant (amendment 3): a v3 file with
    /// one model recorded at two different dims across scopes is LEGAL v3
    /// state (the old RAM slab pinned dims per-`(model, scope)`, not
    /// per-model) hitting the v4 per-model-only policy — `Rejected`, naming
    /// the model and both dims, never `Encoding`.
    #[test]
    fn one_model_two_dims_across_scopes_rejects_not_encoding() {
        let a = NodeId::from_u128(1);
        let b = NodeId::from_u128(2);
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("t.redb")).unwrap();
        let (slot_a, slot_b, scope_a, scope_b);
        {
            let tx = db.begin_write().unwrap();
            {
                let mut meta = tx.open_table(META).unwrap();
                let mut node_slots = tx.open_table(NODE_SLOTS).unwrap();
                let mut node_ids = tx.open_table(NODE_IDS).unwrap();
                alloc_node_slot(&mut meta, &mut node_slots, &mut node_ids, a).unwrap();
                alloc_node_slot(&mut meta, &mut node_slots, &mut node_ids, b).unwrap();
                slot_a = node_slot(&node_slots, a).unwrap().unwrap();
                slot_b = node_slot(&node_slots, b).unwrap().unwrap();

                let mut scopes_table = tx.open_table(SCOPES).unwrap();
                seed_shared(&mut scopes_table).unwrap();
                let mut scopes = ScopeRegistry::load_table_for_rebuild(&scopes_table).unwrap();
                let mut dict_table = tx.open_table(DICT).unwrap();
                let mut dicts = Dicts::default();
                let mut nodes = tx.open_table(NODES).unwrap();
                let scope_x = Scope::Id(crate::ids::ScopeId::from_u128(10));
                let scope_y = Scope::Id(crate::ids::ScopeId::from_u128(20));
                for (id, slot, scope) in [(a, slot_a, scope_x), (b, slot_b, scope_y)] {
                    let rec = NodeRecord {
                        id,
                        scope,
                        label: "M".into(),
                        props: Default::default(),
                        embedding: None,
                    };
                    let disk = node_to_disk_v3(
                        &rec,
                        &mut dict_table,
                        &mut dicts,
                        &mut scopes_table,
                        &mut scopes,
                    )
                    .unwrap();
                    let raw = postcard::to_allocvec(&disk).unwrap();
                    let framed = frame_value(raw);
                    nodes
                        .insert(slot_key(slot).as_slice(), framed.as_slice())
                        .unwrap();
                }
                scope_a = scopes.intern(&mut scopes_table, scope_x).unwrap();
                scope_b = scopes.intern(&mut scopes_table, scope_y).unwrap();

                let mut embeddings = tx.open_table(EMBEDDINGS).unwrap();
                for (slot, dim) in [(slot_a, 2usize), (slot_b, 3usize)] {
                    let raw =
                        postcard::to_allocvec(&("shared-model".to_string(), vec![1.0f32; dim]))
                            .unwrap();
                    let framed = frame_value(raw);
                    embeddings
                        .insert(slot_key(slot).as_slice(), framed.as_slice())
                        .unwrap();
                }
                tx.open_table(VECTOR_DIMS).unwrap();
                tx.open_table(VECTORS).unwrap();
                tx.open_table(EMBEDDING_REF).unwrap();
                tx.open_table(crate::storage::POSTINGS).unwrap();
            }
            tx.commit().unwrap();
        }
        let _ = (scope_a, scope_b);

        let tx = db.begin_write().unwrap();
        let err = {
            let nodes = tx.open_table(NODES).unwrap();
            let embeddings = tx.open_table(EMBEDDINGS).unwrap();
            let mut vector_dims = tx.open_table(VECTOR_DIMS).unwrap();
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut embedding_ref = tx.open_table(EMBEDDING_REF).unwrap();
            let mut dict_table = tx.open_table(DICT).unwrap();
            let mut dicts = Dicts::default();
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            migrate_v3_to_v4(
                &embeddings,
                &nodes,
                &mut vector_dims,
                &mut vectors,
                &mut embedding_ref,
                &mut dict_table,
                &mut dicts,
                &mut postings,
                true,
            )
            .unwrap_err()
        };
        match &err {
            TopoError::Rejected(msg) => {
                assert!(
                    msg.contains("shared-model"),
                    "message must name the model, got {msg:?}"
                );
                assert!(
                    msg.contains('2') && msg.contains('3'),
                    "message must name both dims, got {msg:?}"
                );
            }
            other => panic!("expected Rejected (not Encoding), got {other:?}"),
        }
    }

    #[test]
    fn postings_pass_rechunks_single_row_v3_postings() {
        let id = NodeId::new();
        let (_dir, db, _slot, scope_id) = rig(id);

        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            let entries = vec![(2u64, 1u32), (5, 2), (9, 1)];
            let framed = frame_value(encode_v3_posting_value(&entries));
            postings
                .insert(
                    v3_posting_key(scope_id, "rust").as_slice(),
                    framed.as_slice(),
                )
                .unwrap();
        }
        {
            let nodes = tx.open_table(NODES).unwrap();
            let embeddings = tx.open_table(EMBEDDINGS).unwrap();
            let mut vector_dims = tx.open_table(VECTOR_DIMS).unwrap();
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut embedding_ref = tx.open_table(EMBEDDING_REF).unwrap();
            let mut dict_table = tx.open_table(DICT).unwrap();
            let mut dicts = Dicts::default();
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            migrate_v3_to_v4(
                &embeddings,
                &nodes,
                &mut vector_dims,
                &mut vectors,
                &mut embedding_ref,
                &mut dict_table,
                &mut dicts,
                &mut postings,
                false,
            )
            .unwrap();

            // The old single-row key must be gone...
            assert!(postings
                .get(v3_posting_key(scope_id, "rust").as_slice())
                .unwrap()
                .is_none());
            // ...replaced by a chunked key holding the same entries.
            assert!(postings
                .get(chunked_posting_key(scope_id, "rust", 0).as_slice())
                .unwrap()
                .is_some());
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let postings = tx.open_table(crate::storage::POSTINGS).unwrap();
        assert_eq!(
            read_posting(&postings, scope_id, "rust").unwrap(),
            vec![(2, 1), (5, 2), (9, 1)]
        );
        assert_eq!(posting_df(&postings, scope_id, "rust").unwrap(), 3);
    }

    #[test]
    fn postings_already_chunked_flag_skips_the_postings_pass() {
        let id = NodeId::new();
        let (_dir, db, _slot, scope_id) = rig(id);

        // Write a REAL chunked row directly (as migrate_v2_to_v3's fts_update
        // would under this build) — if the postings pass ran anyway, trying
        // to decode this as an old single-row value would corrupt or error.
        let tx = db.begin_write().unwrap();
        {
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            crate::fts::set_posting(&mut postings, scope_id, "already-chunked", 3, 1).unwrap();
        }
        let before: Vec<u8> = {
            let postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            let guard = postings
                .get(chunked_posting_key(scope_id, "already-chunked", 0).as_slice())
                .unwrap()
                .unwrap();
            let bytes = guard.value().to_vec();
            bytes
        };
        {
            let nodes = tx.open_table(NODES).unwrap();
            let embeddings = tx.open_table(EMBEDDINGS).unwrap();
            let mut vector_dims = tx.open_table(VECTOR_DIMS).unwrap();
            let mut vectors = tx.open_table(VECTORS).unwrap();
            let mut embedding_ref = tx.open_table(EMBEDDING_REF).unwrap();
            let mut dict_table = tx.open_table(DICT).unwrap();
            let mut dicts = Dicts::default();
            let mut postings = tx.open_table(crate::storage::POSTINGS).unwrap();
            migrate_v3_to_v4(
                &embeddings,
                &nodes,
                &mut vector_dims,
                &mut vectors,
                &mut embedding_ref,
                &mut dict_table,
                &mut dicts,
                &mut postings,
                true, // already chunked — postings pass must be a no-op
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let postings = tx.open_table(crate::storage::POSTINGS).unwrap();
        let after = postings
            .get(chunked_posting_key(scope_id, "already-chunked", 0).as_slice())
            .unwrap()
            .unwrap()
            .value()
            .to_vec();
        assert_eq!(
            before, after,
            "postings_already_chunked=true must leave POSTINGS byte-identical"
        );
    }
}
