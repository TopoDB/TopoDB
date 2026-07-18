use std::collections::BTreeMap;

use crate::runner::{AgentRunner, NodeOutcome, NodeRequest};
use crate::schema::validate::Validated;
use crate::schema::NodeKind;
use crate::store::run::{NodeState, RunStore};
use crate::store::SghError;

#[derive(Debug, Default, PartialEq)]
pub struct RunReport {
    pub succeeded: Vec<String>,
    pub blocked: Vec<String>,
    pub skipped: Vec<String>,
    pub model_calls: u64,
}

/// Advances every node of a `Validated` graph through the state machine,
/// sequentially, in topological order. The executor never invents
/// structure: it only ever consults the graph and store it was given, and
/// a node is only ever handed the outputs of its own declared dependencies
/// (see `execute_node`'s `inputs` assembly). Recovery is deliberately not
/// implemented here — a failed node goes straight to `Blocked`; Task 9
/// inserts the retry/repair ladder on top of this.
pub struct Executor<'r> {
    store: RunStore,
    graph: Validated,
    runner: &'r dyn AgentRunner,
    clock: i64,
    model_calls: u64,
}

impl<'r> Executor<'r> {
    pub fn new(store: RunStore, graph: Validated, runner: &'r dyn AgentRunner) -> Self {
        Executor { store, graph, runner, clock: 0, model_calls: 0 }
    }

    /// Every write advances a logical clock rather than reading wall time, so
    /// a run's timeline is reproducible.
    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    pub fn run(&mut self, start_ms: i64) -> Result<RunReport, SghError> {
        self.clock = start_ms;

        // Topological order makes a single forward pass sufficient: every
        // dependency is resolved (or has failed and been skipped) before its
        // dependents are considered.
        let order = self.graph.topo_order.clone();
        for id in order {
            let deps = self.graph.graph.node(&id).expect("node exists").needs.clone();

            let mut any_dep_unfinished = false;
            for d in &deps {
                if self.store.state(d)? != NodeState::Succeeded {
                    any_dep_unfinished = true;
                    break;
                }
            }

            if any_dep_unfinished {
                let t = self.tick();
                self.store.set_state(&id, NodeState::Skipped, t)?;
                continue;
            }

            let t = self.tick();
            self.store.set_state(&id, NodeState::Ready, t)?;

            self.execute_node(&id)?;
        }

        self.report()
    }

    fn execute_node(&mut self, id: &str) -> Result<(), SghError> {
        let node = self.graph.graph.node(id).expect("node exists").clone();

        // Gate nodes halt the run for human approval; there is no
        // interactive surface yet, so a gate simply blocks.
        if node.kind == NodeKind::Gate {
            let t = self.tick();
            self.store.set_state(id, NodeState::Blocked, t)?;
            return Ok(());
        }

        // Bounded context: inputs are exactly the outputs of this node's
        // declared dependencies. Nothing else in the run is reachable from
        // here, and this map is the only channel through which a node sees
        // prior work.
        let mut inputs = BTreeMap::new();
        for dep in &node.needs {
            if let Some(out) = self.store.output(dep)? {
                inputs.insert(dep.clone(), out);
            }
        }

        let req = NodeRequest {
            node_id: id.to_string(),
            prompt: node.prompt.clone().or(node.run.clone()).unwrap_or_default(),
            inputs,
            output_schema: node.output.as_ref().map(|o| o.schema.clone()),
        };

        let t = self.tick();
        self.store.set_state(id, NodeState::Running, t)?;

        if node.kind == NodeKind::Agent {
            self.model_calls += 1;
        }

        let outcome = match self.runner.run(&req) {
            Ok(o) => o,
            Err(e) => NodeOutcome::Failed { error: e.to_string() },
        };

        match outcome {
            NodeOutcome::Succeeded { output } => match validate_output(&node, &output) {
                Ok(()) => {
                    let t = self.tick();
                    self.store.record_output(id, &output, t)?;
                    let t = self.tick();
                    self.store.set_state(id, NodeState::Succeeded, t)?;
                }
                Err(reason) => {
                    let t = self.tick();
                    self.store.record_attempt(id, "execute", &reason, t)?;
                    let t = self.tick();
                    self.store.set_state(id, NodeState::Blocked, t)?;
                }
            },
            NodeOutcome::Failed { error } => {
                let t = self.tick();
                self.store.record_attempt(id, "execute", &error, t)?;
                let t = self.tick();
                self.store.set_state(id, NodeState::Blocked, t)?;
            }
        }

        Ok(())
    }

    fn report(&self) -> Result<RunReport, SghError> {
        let mut r = RunReport { model_calls: self.model_calls, ..Default::default() };
        for id in &self.graph.topo_order {
            match self.store.state(id)? {
                NodeState::Succeeded => r.succeeded.push(id.clone()),
                NodeState::Blocked => r.blocked.push(id.clone()),
                NodeState::Skipped => r.skipped.push(id.clone()),
                _ => {}
            }
        }
        Ok(r)
    }
}

/// A node returning output that does not match its declared schema has
/// failed — this is what makes declared outputs load-bearing rather than
/// documentation. A node with no declared output schema is unconstrained.
fn validate_output(node: &crate::schema::Node, output: &str) -> Result<(), String> {
    let Some(spec) = &node.output else { return Ok(()) };
    let value: serde_json::Value =
        serde_json::from_str(output).map_err(|e| format!("output is not valid json: {e}"))?;
    let compiled = jsonschema::JSONSchema::compile(&spec.schema)
        .map_err(|e| format!("output schema does not compile: {e}"))?;
    if let Err(errors) = compiled.validate(&value) {
        let msgs: Vec<String> = errors.map(|e| e.to_string()).collect();
        return Err(format!("output does not match schema: {}", msgs.join("; ")));
    }
    Ok(())
}
