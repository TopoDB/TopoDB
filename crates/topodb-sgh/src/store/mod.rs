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
/// A proposed successor graph for a run, written by the replan step and
/// linked `SghRun -[REVISION_OF]-> SghRevision`. Superseding, so a run
/// carries at most one open proposal while earlier ones stay in history.
/// The edge is keyed from the run, not the revision, because supersession
/// keys on `(from, ty)` and must be anchored on the run's stable node id —
/// a fresh revision id would never match a prior edge, so it could never
/// close it out.
pub const LABEL_REVISION: &str = "SghRevision";

pub const EDGE_DEPENDS_ON: &str = "DEPENDS_ON";
pub const EDGE_HAS_STATE: &str = "HAS_STATE";
/// `node -[EDGE_PRODUCED]-> SghOutput`. Keyed on the stable node id so
/// `link_superseding` can close the prior output edge when a node produces a
/// new output (see `RunStore::record_output`).
pub const EDGE_PRODUCED: &str = "PRODUCED";
pub const EDGE_ATTEMPT_OF: &str = "ATTEMPT_OF";
/// Links a proposed successor graph to the run that produced it.
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
        "graph contains command nodes but no CommandRunner was configured: {nodes:?}; call \
         Executor::with_command_runner"
    )]
    NoCommandRunner { nodes: Vec<String> },
}
