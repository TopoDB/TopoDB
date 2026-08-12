use crate::ids::{EdgeId, NodeId, Scope};
use crate::props::{PropValue, Props};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::collections::BTreeMap;

/// A fully-resolved mutation. INVARIANT: ops are appended to the log only
/// after the applier resolves every default (timestamps especially) — a
/// stored op never contains "now". Replay must be deterministic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    CreateNode {
        id: NodeId,
        scope: Scope,
        label: SmolStr,
        props: Props,
    },
    /// `None` value removes the key. Overwrites current state (edge-level
    /// temporality doctrine — see spec).
    SetNodeProps {
        id: NodeId,
        props: BTreeMap<String, Option<PropValue>>,
    },
    SetEmbedding {
        id: NodeId,
        model: String,
        vector: Vec<f32>,
    },
    /// Hard delete; applier also removes incident edges (deterministic:
    /// derived from state built by prior ops).
    RemoveNode { id: NodeId },
    CreateEdge {
        id: EdgeId,
        scope: Scope,
        ty: SmolStr,
        from: NodeId,
        to: NodeId,
        props: Props,
        /// ALWAYS Some(...) once stored (applier resolves).
        valid_from: Option<i64>,
        /// Belief time; ALWAYS Some(write instant) once stored — resolve
        /// overwrites caller input. Absent in pre-v9 log entries; apply
        /// derives valid_from.
        recorded_at: Option<i64>,
    },
    CloseEdge {
        id: EdgeId,
        valid_to: Option<i64>,
        /// Belief time; ALWAYS Some(write instant) once stored — resolve
        /// overwrites caller input. Absent in pre-v9 log entries; apply
        /// derives valid_to.
        superseded_at: Option<i64>,
    },
    /// Atomic find-or-create by a unique, equality-indexed prop. Resolved at
    /// APPLY time (in the single applier's transaction, so it sees every
    /// prior-committed and prior-same-group create): if a node with `label` and
    /// `key_prop == props[key_prop]` already exists in `scope`, this op is
    /// dropped and `id` is remapped to the existing node's id for the rest of
    /// the batch (edges to `id` follow); otherwise it applies exactly like
    /// `CreateNode { id, scope, label, props }`. This is the concurrency-safe
    /// alternative to a plan-time find-then-CreateNode, which fragments the
    /// graph under concurrent writers (two writers both read "absent" and each
    /// create a node).
    ///
    /// INVARIANT: an `UpsertNode` is ALWAYS resolved into a plain `CreateNode`
    /// (or dropped) before the op log is appended, so a stored/replayed log
    /// never contains this variant — which is why it is placed LAST here: a
    /// transient, apply-only op must never shift the postcard discriminants of
    /// the persisted variants above it.
    UpsertNode {
        id: NodeId,
        scope: Scope,
        label: SmolStr,
        key_prop: SmolStr,
        props: Props,
    },
}
