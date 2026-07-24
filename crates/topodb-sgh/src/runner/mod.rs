pub mod claude;
pub mod command;
pub mod mock;

use std::collections::BTreeMap;

/// Everything a node is allowed to see. Inputs are exactly the declared
/// upstream outputs — nothing else from the run is reachable, which is how
/// bounded context is enforced structurally rather than by convention.
#[derive(Debug, Clone)]
pub struct NodeRequest {
    pub node_id: String,
    pub prompt: String,
    /// upstream node id -> that node's output JSON
    pub inputs: BTreeMap<String, String>,
    pub output_schema: Option<serde_json::Value>,
    /// MCP tool surfaces the node opted into (`Node.tools`, validated
    /// upstream). The runner maps these to `--allowedTools` entries; empty
    /// means the node gets no MCP surface even when the run supplies one.
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeOutcome {
    Succeeded { output: String },
    Failed { error: String },
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("runner io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("runner produced invalid utf-8")]
    Utf8,
}

pub trait AgentRunner: Send + Sync {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError>;
}
