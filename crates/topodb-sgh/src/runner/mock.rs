use std::collections::HashMap;
use std::sync::Mutex;

use super::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};

/// A runner that returns scripted outcomes and never calls a model. This is
/// what makes the executor, the state machine, and the recovery ladder
/// testable deterministically in CI at zero token cost.
#[derive(Default)]
pub struct MockRunner {
    scripts: Mutex<HashMap<String, Vec<NodeOutcome>>>,
    cursors: Mutex<HashMap<String, usize>>,
    calls: Mutex<Vec<String>>,
}

impl MockRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue outcomes for a node. Once the script is exhausted the last
    /// outcome repeats forever, so a permanently-failing node is one entry.
    pub fn script(self, node_id: &str, outcomes: Vec<NodeOutcome>) -> Self {
        self.scripts
            .lock()
            .unwrap()
            .insert(node_id.to_string(), outcomes);
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl AgentRunner for MockRunner {
    fn run(&self, req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        self.calls.lock().unwrap().push(req.node_id.clone());

        let scripts = self.scripts.lock().unwrap();
        let Some(outcomes) = scripts.get(&req.node_id) else {
            return Ok(NodeOutcome::Succeeded { output: "{}".to_string() });
        };
        if outcomes.is_empty() {
            return Ok(NodeOutcome::Succeeded { output: "{}".to_string() });
        }

        let mut cursors = self.cursors.lock().unwrap();
        let cursor = cursors.entry(req.node_id.clone()).or_insert(0);
        let idx = (*cursor).min(outcomes.len() - 1);
        *cursor += 1;
        Ok(outcomes[idx].clone())
    }
}
