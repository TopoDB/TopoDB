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
/// A shared-scope, cross-run index record: one per run, created alongside
/// the run itself (`RunStore::create`) and updated in place by
/// `RunStore::set_status`. Deliberately NOT linked by edge to anything in the
/// run's own `Scope::Id` — the engine's scoping model treats each concrete
/// scope as its own island for edge endpoints (see `link_superseding`'s doc
/// comment on why a bare `ScopeSet::of(&[sid])` is used for the run's own
/// reads), so cross-scope edges are not a supported way to join shared and
/// per-run data. The join key is data, not structure: `run_id` and
/// `scope_id` are plain string props, and a reader who wants "the run this
/// index describes" parses `scope_id` back into a `ScopeId` and opens a
/// fresh `ScopeSet::of(&[that_id])`.
///
/// `ScopeId` finding (binds Task 4's `RunStore::open`): `topodb::ScopeId`
/// (via the crate's `id_type!` macro, `crates/topodb/src/ids.rs`) has a full
/// `Display`/`FromStr` pair delegating to `ulid::Ulid`'s canonical Crockford
/// base32 string form, and it round-trips exactly (`s.parse::<ScopeId>()`).
/// So the index stores `scope_id: Str(scope_id.to_string())` ONLY — no
/// parallel `Bytes` representation is needed, and Task 4's `open` should
/// prefer `scope_id_str.parse::<ScopeId>()` over any byte-level accessor.
pub const LABEL_RUN_INDEX: &str = "SghRunIndex";

pub const RUN_STATUS_RUNNING: &str = "running";
pub const RUN_STATUS_COMPLETE: &str = "complete";
pub const RUN_STATUS_BLOCKED: &str = "blocked";
pub const RUN_STATUS_CHECKPOINT: &str = "checkpoint";

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
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("corrupt run {run_id}: {reason}")]
    CorruptRun { run_id: String, reason: String },
    #[error("run {run_id:?} not found in this database")]
    RunNotFound { run_id: String },
    #[error(
        "graph contains command nodes but no CommandRunner was configured: {nodes:?}; call \
         Executor::with_command_runner"
    )]
    NoCommandRunner { nodes: Vec<String> },
    #[error("worker panicked while executing node {node}")]
    WorkerPanic { node: String },
}
