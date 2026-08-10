//! v8 -> v9: bi-temporal edges. Postcard is positional, so both the EDGES
//! rows (`EdgeRecordDiskV3` -> `EdgeRecordDiskV4`) and the stored
//! `CreateEdge`/`CloseEdge` ops must be rewritten. Backfill is the approved
//! copy rule: `recorded_at := valid_from`, `superseded_at := valid_to` —
//! exact for every edge that was never explicitly backdated, a documented
//! approximation otherwise (see the design spec's "Migration" section). One-
//! way, in place, same drain-then-rewrite shape as
//! `migrate_v8::quantize_vectors` (redb tables cannot mutate mid-iteration).
//!
//! Two independent postcard-decode directions matter here, and this module
//! exercises both:
//!
//! 1. **New bytes, old decoder**: decoding a V4-shaped `EdgeRecordDiskV4` row
//!    (or a current `Op::CreateEdge`/`Op::CloseEdge`, which now carry
//!    trailing `recorded_at`/`superseded_at` fields) with the correspondingly
//!    OLDER decoder (`EdgeRecordDiskV3` / `OpDiskV8` below) does **NOT**
//!    error — postcard is positional and has no self-description, so it
//!    happily decodes the shared PREFIX fields and silently ignores the
//!    trailing bytes. This is the Task 1 ARC lesson: it means a decode that
//!    "succeeds" against a stale shape proves nothing about whether the
//!    stored bytes were actually rewritten.
//! 2. **Old bytes, new decoder**: decoding an OLDER, shorter row (a genuine
//!    pre-v9 `EdgeRecordDiskV3` row, or a pre-v9 `Op::CreateEdge`/
//!    `Op::CloseEdge` missing the belief-axis field) with the CURRENT,
//!    LONGER decoder (`EdgeRecordDiskV4` / `Op`) DOES error — postcard hits
//!    EOF looking for the trailing field that was never written. Verified
//!    empirically by this module's `old_op_bytes_error_under_new_decoder`
//!    test.
//!
//! Direction 2 is exactly why the OPS rewrite below must exist as real data
//! motion, not a no-op stamp: `rebuild_state_from_ops`/`read_ops` decode
//! every stored op with the CURRENT `Op` shape unconditionally (there is no
//! version-gated op decoder on the live path) — so a v9-stamped file that
//! still had pre-v9-shaped op bytes on disk would fail replay/the change
//! feed the moment it hit an old `CreateEdge`/`CloseEdge` entry, direction 2
//! above. This module's job is to make that impossible: every op the OPS
//! table can produce, from any pre-v9 log, must decode under the CURRENT
//! `Op` shape (with `Some(...)` belief fields) after `bitemporalize` runs.
use crate::codec::{frame_value, unframe_value};
use crate::disk::{EdgeRecordDiskV3, EdgeRecordDiskV4};
use crate::error::{storage_err, TopoError};
use crate::ids::{EdgeId, NodeId, Scope};
use crate::op::Op;
use crate::props::{PropValue, Props};
use crate::storage::{EDGES, OPS};
use redb::{ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::collections::BTreeMap;

/// Frozen twin of the PRE-Task-1 `Op` enum (`git show
/// a8a7d6e:crates/topodb/src/op.rs`) — the shape every op stored before this
/// migration was written under. Variant order, field sets and field order
/// must match that commit EXACTLY for postcard to decode existing OPS rows
/// correctly (postcard is positional: it has no variant/field names on the
/// wire, only ordinal tags). Never edit this to track the live `Op` enum —
/// it must stay byte-compatible with pre-v9 logs forever, the same
/// "frozen decode twin" discipline `disk.rs` documents for
/// `EdgeRecordDiskV3`.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum OpDiskV8 {
    CreateNode {
        id: NodeId,
        scope: Scope,
        label: SmolStr,
        props: Props,
    },
    SetNodeProps {
        id: NodeId,
        props: BTreeMap<String, Option<PropValue>>,
    },
    SetEmbedding {
        id: NodeId,
        model: String,
        vector: Vec<f32>,
    },
    RemoveNode {
        id: NodeId,
    },
    CreateEdge {
        id: EdgeId,
        scope: Scope,
        ty: SmolStr,
        from: NodeId,
        to: NodeId,
        props: Props,
        valid_from: Option<i64>,
    },
    CloseEdge {
        id: EdgeId,
        valid_to: Option<i64>,
    },
}

/// Converts a decoded pre-v9 op into the current `Op` shape. `CreateEdge`
/// gains `recorded_at := valid_from` (the stored op's own resolved
/// `valid_from` — every pre-v9 stored op has `valid_from: Some(_)`, since
/// `resolve_op` always resolves it before append); `CloseEdge` gains
/// `superseded_at := valid_to`, same rule. This upholds the replay/migration
/// agreement documented on `Op::CreateEdge`/`Op::CloseEdge`: replaying a
/// pre-v9 log through `apply_op`'s copy-rule fallback and migrating a pre-v9
/// FILE must produce byte-identical `recorded_at`/`superseded_at`. Every
/// other variant passes through unchanged, just re-encoded under the current
/// enum — no dual-decode paths survive outside this file.
fn convert_op(old: OpDiskV8) -> Op {
    match old {
        OpDiskV8::CreateNode {
            id,
            scope,
            label,
            props,
        } => Op::CreateNode {
            id,
            scope,
            label,
            props,
        },
        OpDiskV8::SetNodeProps { id, props } => Op::SetNodeProps { id, props },
        OpDiskV8::SetEmbedding { id, model, vector } => Op::SetEmbedding { id, model, vector },
        OpDiskV8::RemoveNode { id } => Op::RemoveNode { id },
        OpDiskV8::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from,
        } => Op::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from,
            recorded_at: valid_from,
        },
        OpDiskV8::CloseEdge { id, valid_to } => Op::CloseEdge {
            id,
            valid_to,
            superseded_at: valid_to,
        },
    }
}

/// Drains and rewrites both the EDGES and OPS tables in place: EDGES rows
/// V3 -> V4 (copy-rule backfill), OPS rows `OpDiskV8` -> current `Op`
/// (`CreateEdge`/`CloseEdge` gain `Some(...)` belief fields via the same
/// copy rule; every other variant re-encoded verbatim). Keys+values are
/// collected before rewriting in each pass (redb tables cannot be mutated
/// mid-iteration), same tradeoff `migrate_v8::quantize_vectors` documents.
pub(crate) fn bitemporalize(tx: &WriteTransaction) -> Result<(), TopoError> {
    // ---- EDGES: V3 -> V4 copy-rule backfill ----
    {
        let mut table = tx.open_table(EDGES).map_err(storage_err)?;
        let mut rewrites: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for entry in table.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let raw = unframe_value(v.value())?;
            let old: EdgeRecordDiskV3 = postcard::from_bytes(&raw)
                .map_err(|e| TopoError::Encoding(format!("v9 edges migration: {e}")))?;
            let new = EdgeRecordDiskV4 {
                id: old.id,
                scope: old.scope,
                ty: old.ty,
                from: old.from,
                to: old.to,
                props: old.props,
                valid_from: old.valid_from,
                valid_to: old.valid_to,
                recorded_at: old.valid_from,
                superseded_at: old.valid_to,
            };
            let out = postcard::to_allocvec(&new)
                .map_err(|e| TopoError::Encoding(format!("v9 edges migration: {e}")))?;
            rewrites.push((k.value().to_vec(), frame_value(out)));
        }
        for (k, v) in rewrites {
            table
                .insert(k.as_slice(), v.as_slice())
                .map_err(storage_err)?;
        }
    }

    // ---- OPS: OpDiskV8 -> current Op (CreateEdge/CloseEdge gain belief axis) ----
    {
        let mut table = tx.open_table(OPS).map_err(storage_err)?;
        let mut rewrites: Vec<(u64, Vec<u8>)> = Vec::new();
        for entry in table.iter().map_err(storage_err)? {
            let (k, v) = entry.map_err(storage_err)?;
            let old: OpDiskV8 = postcard::from_bytes(v.value())
                .map_err(|e| TopoError::Encoding(format!("v9 ops migration: {e}")))?;
            let new = convert_op(old);
            let out = postcard::to_allocvec(&new)
                .map_err(|e| TopoError::Encoding(format!("v9 ops migration: {e}")))?;
            rewrites.push((k.value(), out));
        }
        for (k, v) in rewrites {
            table.insert(k, v.as_slice()).map_err(storage_err)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ScopeId;

    /// Empirical proof of direction 2 (see module doc comment): a genuinely
    /// old-shaped `CreateEdge`/`CloseEdge` op — missing the trailing belief-
    /// axis field entirely — errors when decoded with the CURRENT `Op`
    /// decoder. This is exactly why the OPS rewrite pass above must exist as
    /// real data motion: without it, a v9-stamped file could still contain
    /// bytes that fail this decode on the very first replay/change-feed read.
    #[test]
    fn old_op_bytes_error_under_new_decoder() {
        let old_create = OpDiskV8::CreateEdge {
            id: EdgeId::new(),
            scope: Scope::Id(ScopeId::new()),
            ty: "knows".into(),
            from: NodeId::new(),
            to: NodeId::new(),
            props: Props::new(),
            valid_from: Some(1_000),
        };
        let bytes = postcard::to_allocvec(&old_create).unwrap();
        let decoded: Result<Op, _> = postcard::from_bytes(&bytes);
        assert!(
            decoded.is_err(),
            "old-shaped CreateEdge bytes must NOT decode under the current Op shape"
        );

        let old_close = OpDiskV8::CloseEdge {
            id: EdgeId::new(),
            valid_to: Some(2_000),
        };
        let bytes = postcard::to_allocvec(&old_close).unwrap();
        let decoded: Result<Op, _> = postcard::from_bytes(&bytes);
        assert!(
            decoded.is_err(),
            "old-shaped CloseEdge bytes must NOT decode under the current Op shape"
        );
    }

    /// Same direction-2 proof for the EDGES row shape: a genuine V3 row
    /// (no belief axis at all) errors under the V4 decoder — confirming the
    /// EDGES rewrite pass is likewise load-bearing, not cosmetic.
    #[test]
    fn old_edge_bytes_error_under_new_decoder() {
        let old = EdgeRecordDiskV3 {
            id: EdgeId::new(),
            scope: 1,
            ty: 1,
            from: 1,
            to: 2,
            props: BTreeMap::new(),
            valid_from: 1_000,
            valid_to: None,
        };
        let bytes = postcard::to_allocvec(&old).unwrap();
        let decoded: Result<EdgeRecordDiskV4, _> = postcard::from_bytes(&bytes);
        assert!(
            decoded.is_err(),
            "old-shaped EdgeRecordDiskV3 bytes must NOT decode under EdgeRecordDiskV4"
        );
    }

    /// Direction 1 (see module doc comment), for completeness: NEW bytes
    /// decode "successfully" under the OLD decoder, silently dropping the
    /// trailing belief-axis fields — proving that a decode which merely
    /// "succeeds" is not evidence of a correct migration. This is why the
    /// fixture test (`format_fixture.rs`) asserts `Some(...)` field VALUES,
    /// not just decode success.
    #[test]
    fn new_op_bytes_decode_silently_under_old_decoder() {
        let new_create = Op::CreateEdge {
            id: EdgeId::new(),
            scope: Scope::Id(ScopeId::new()),
            ty: "knows".into(),
            from: NodeId::new(),
            to: NodeId::new(),
            props: Props::new(),
            valid_from: Some(1_000),
            recorded_at: Some(1_000),
        };
        let bytes = postcard::to_allocvec(&new_create).unwrap();
        let decoded: OpDiskV8 = postcard::from_bytes(&bytes)
            .expect("new-shaped bytes must decode 'successfully' under the old decoder");
        match decoded {
            OpDiskV8::CreateEdge { valid_from, .. } => assert_eq!(valid_from, Some(1_000)),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn bitemporalize_backfills_edges_and_rewrites_ops_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        let edge_id = EdgeId::new();
        {
            let mut edges = tx.open_table(EDGES).unwrap();
            let old = EdgeRecordDiskV3 {
                id: edge_id,
                scope: 1,
                ty: 7,
                from: 1,
                to: 2,
                props: BTreeMap::new(),
                valid_from: 1_000,
                valid_to: Some(2_000),
            };
            let raw = postcard::to_allocvec(&old).unwrap();
            edges
                .insert(1u64.to_be_bytes().as_slice(), frame_value(raw).as_slice())
                .unwrap();

            let mut ops = tx.open_table(OPS).unwrap();
            let op = OpDiskV8::CreateEdge {
                id: edge_id,
                scope: Scope::Id(ScopeId::new()),
                ty: "knows".into(),
                from: NodeId::new(),
                to: NodeId::new(),
                props: Props::new(),
                valid_from: Some(1_000),
            };
            ops.insert(1u64, postcard::to_allocvec(&op).unwrap().as_slice())
                .unwrap();
            let close = OpDiskV8::CloseEdge {
                id: edge_id,
                valid_to: Some(2_000),
            };
            ops.insert(2u64, postcard::to_allocvec(&close).unwrap().as_slice())
                .unwrap();
        }
        bitemporalize(&tx).unwrap();
        {
            let edges = tx.open_table(EDGES).unwrap();
            let raw = edges.get(1u64.to_be_bytes().as_slice()).unwrap().unwrap();
            let unframed = unframe_value(raw.value()).unwrap();
            let decoded: EdgeRecordDiskV4 = postcard::from_bytes(&unframed).unwrap();
            assert_eq!(decoded.recorded_at, 1_000);
            assert_eq!(decoded.superseded_at, Some(2_000));

            let ops = tx.open_table(OPS).unwrap();
            let raw = ops.get(1u64).unwrap().unwrap();
            let decoded: Op = postcard::from_bytes(raw.value()).unwrap();
            match decoded {
                Op::CreateEdge { recorded_at, .. } => assert_eq!(recorded_at, Some(1_000)),
                other => panic!("unexpected op: {other:?}"),
            }
            let raw = ops.get(2u64).unwrap().unwrap();
            let decoded: Op = postcard::from_bytes(raw.value()).unwrap();
            match decoded {
                Op::CloseEdge { superseded_at, .. } => assert_eq!(superseded_at, Some(2_000)),
                other => panic!("unexpected op: {other:?}"),
            }
        }
        tx.commit().unwrap();
    }
}
