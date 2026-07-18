pub mod run;
pub mod supersede;

/// Node labels used by sgh. These are sgh's own vocabulary — "Entity" and
/// "Memory" are conventions of `topodb-json`, not engine concepts, and sgh
/// deliberately does not adopt them.
pub const LABEL_RUN: &str = "SghRun";
pub const LABEL_NODE: &str = "SghNode";
pub const LABEL_STATE: &str = "SghState";
pub const LABEL_OUTPUT: &str = "SghOutput";
pub const LABEL_ATTEMPT: &str = "SghAttempt";
// TODO(v0.0.2): reserved for persisting repair revisions (the sequence of
// `Node`s a REPAIR rung produces via `Repairer::repair`) so a run's history
// records not just that a node was repaired but what it was repaired to.
// Not yet written or read anywhere.
pub const LABEL_REVISION: &str = "SghRevision";

pub const EDGE_DEPENDS_ON: &str = "DEPENDS_ON";
pub const EDGE_HAS_STATE: &str = "HAS_STATE";
/// `node -[EDGE_PRODUCED]-> SghOutput`. Keyed on the stable node id so
/// `link_superseding` can close the prior output edge when a node produces a
/// new output (see `RunStore::record_output`).
pub const EDGE_PRODUCED: &str = "PRODUCED";
pub const EDGE_ATTEMPT_OF: &str = "ATTEMPT_OF";
// TODO(v0.0.2): the `SghRevision -[EDGE_REVISION_OF]-> SghNode` edge for the
// same not-yet-implemented repair-revision history as `LABEL_REVISION` above.
pub const EDGE_REVISION_OF: &str = "REVISION_OF";
pub const EDGE_MEMBER_OF: &str = "MEMBER_OF";

#[derive(Debug, thiserror::Error)]
pub enum SghError {
    #[error("engine error: {0}")]
    Engine(#[from] topodb::TopoError),
    #[error("supersession lost the race {attempts} times")]
    Contended { attempts: u32 },
    #[error("endpoint node {node:?} does not exist (or is out of scope)")]
    MissingEndpoint { node: topodb::NodeId },
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "command nodes are not supported until v0.0.2: {nodes:?}; command execution has no \
         shell path yet and dispatching them through AgentRunner would send the shell command \
         to a model as a prompt"
    )]
    UnsupportedNodeKind { nodes: Vec<String> },
}
