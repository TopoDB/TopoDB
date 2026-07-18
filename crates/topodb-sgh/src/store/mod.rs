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
pub const LABEL_REVISION: &str = "SghRevision";

pub const EDGE_DEPENDS_ON: &str = "DEPENDS_ON";
pub const EDGE_HAS_STATE: &str = "HAS_STATE";
pub const EDGE_PRODUCED_BY: &str = "PRODUCED_BY";
pub const EDGE_ATTEMPT_OF: &str = "ATTEMPT_OF";
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
}
