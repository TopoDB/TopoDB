use std::sync::Mutex;

use super::{PlanRequest, Planner, PlannerError};
use crate::schema::Graph;

/// A planner that returns scripted YAML and never calls a model, so planner
/// consumers are testable in CI at zero token cost.
pub struct MockPlanner {
    /// `Ok(yaml)` yields that document; `Err(msg)` simulates a backend failure.
    responses: Mutex<Vec<Result<String, String>>>,
    cursor: Mutex<usize>,
}

impl MockPlanner {
    pub fn new(responses: Vec<Result<String, String>>) -> Self {
        MockPlanner {
            responses: Mutex::new(responses),
            cursor: Mutex::new(0),
        }
    }
}

impl Planner for MockPlanner {
    fn plan(&self, _req: &PlanRequest) -> Result<Graph, PlannerError> {
        let responses = self.responses.lock().unwrap();
        let mut cursor = self.cursor.lock().unwrap();
        let idx = (*cursor).min(responses.len().saturating_sub(1));
        *cursor += 1;

        match responses.get(idx) {
            Some(Ok(yaml)) => Graph::from_yaml(yaml).map_err(|e| PlannerError::Yaml(e.to_string())),
            Some(Err(msg)) => Err(PlannerError::Runner(msg.clone())),
            None => Err(PlannerError::Runner("no scripted response".into())),
        }
    }
}
