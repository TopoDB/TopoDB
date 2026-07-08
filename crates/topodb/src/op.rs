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
    CreateNode { id: NodeId, scope: Scope, label: SmolStr, props: Props },
    /// `None` value removes the key. Overwrites current state (edge-level
    /// temporality doctrine — see spec).
    SetNodeProps { id: NodeId, props: BTreeMap<String, Option<PropValue>> },
    SetEmbedding { id: NodeId, model: String, vector: Vec<f32> },
    /// Hard delete; applier also removes incident edges (deterministic:
    /// derived from state built by prior ops).
    RemoveNode { id: NodeId },
    CreateEdge {
        id: EdgeId, scope: Scope, ty: SmolStr,
        from: NodeId, to: NodeId, props: Props,
        /// ALWAYS Some(...) once stored (applier resolves).
        valid_from: Option<i64>,
    },
    CloseEdge { id: EdgeId, valid_to: Option<i64> },
}
