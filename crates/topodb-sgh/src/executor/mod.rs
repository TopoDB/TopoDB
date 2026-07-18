use std::collections::BTreeMap;

use crate::recovery::{contract_preserved, NoopRepairer, Repairer, Rung};
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
/// (see `execute_node`'s `inputs` assembly). A failed node climbs the
/// recovery ladder (retry, then repair, then block) in `execute_node`;
/// REPLAN (regenerating graph structure) is out of scope — the ladder
/// stops at `Blocked`.
pub struct Executor<'r> {
    store: RunStore,
    graph: Validated,
    runner: &'r dyn AgentRunner,
    repairer: &'r dyn Repairer,
    clock: i64,
    model_calls: u64,
}

impl<'r> Executor<'r> {
    pub fn new(store: RunStore, graph: Validated, runner: &'r dyn AgentRunner) -> Self {
        Executor {
            store,
            graph,
            runner,
            repairer: &NoopRepairer,
            clock: 0,
            model_calls: 0,
        }
    }

    /// Wires in a model-backed (or hand-written stub) repairer for the
    /// REPAIR rung. Without one, `NoopRepairer` always declines, so a
    /// contract-preserving revision is never available and the ladder
    /// falls straight from RETRY to BLOCK.
    pub fn with_repairer(mut self, repairer: &'r dyn Repairer) -> Self {
        self.repairer = repairer;
        self
    }

    /// Read-only access to the run store, for inspection and tests.
    pub fn store_ref(&self) -> &RunStore {
        &self.store
    }

    /// Every write advances a logical clock rather than reading wall time, so
    /// a run's timeline is reproducible.
    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    pub fn run(&mut self, start_ms: i64) -> Result<RunReport, SghError> {
        // Command nodes parse, validate, and cost exactly like any other
        // node kind, but there is no shell execution path in this crate yet
        // (see the module doc comment): dispatching one through
        // `AgentRunner` would send a shell command to a model as a prompt,
        // a real model call the cost bound never budgeted for. The CLI
        // (`bin/sgh.rs`) already refuses these graphs with a friendlier
        // message, but `Executor` is public, so the refusal must also live
        // here — otherwise any other library caller could drive the
        // executor straight past its own published bound.
        let offenders: Vec<String> = self
            .graph
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Command)
            .map(|n| n.id.clone())
            .collect();
        if !offenders.is_empty() {
            return Err(SghError::UnsupportedNodeKind { nodes: offenders });
        }

        self.clock = start_ms;

        // Topological order makes a single forward pass sufficient: every
        // dependency is resolved (or has failed and been skipped) before its
        // dependents are considered.
        let order = self.graph.topo_order.clone();
        for id in order {
            let deps = self
                .graph
                .graph
                .node(&id)
                .expect("node exists")
                .needs
                .clone();

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
        let original = self.graph.graph.node(id).expect("node exists").clone();

        // Gate nodes halt the run for human approval; there is no
        // interactive surface yet, so a gate simply blocks.
        if original.kind == NodeKind::Gate {
            let t = self.tick();
            self.store.set_state(id, NodeState::Blocked, t)?;
            return Ok(());
        }

        // Bounded context: inputs are exactly the outputs of this node's
        // declared dependencies. Nothing else in the run is reachable from
        // here, and this map is the only channel through which a node sees
        // prior work.
        let mut inputs = BTreeMap::new();
        for dep in &original.needs {
            if let Some(out) = self.store.output(dep)? {
                inputs.insert(dep.clone(), out);
            }
        }

        // Commands are retry-only: there is no model to consult for a shell
        // invocation, so their repair budget is ignored. Task 3's cost model
        // (`bound.rs`) has no repair term for commands for the same reason.
        let repair_budget = match original.kind {
            NodeKind::Agent => original.budget.repairs,
            _ => 0,
        };

        // `node` is the revisable working copy the ladder operates on; only
        // its prompt ever changes (via a contract-preserving repair).
        // `original` stays untouched so every repair is checked against the
        // node's true, frozen contract, not against the last revision.
        let mut node = original.clone();
        let mut retries_left = original.budget.retries;
        let mut repairs_left = repair_budget;

        loop {
            let req = NodeRequest {
                node_id: id.to_string(),
                prompt: node.prompt.clone().or(node.run.clone()).unwrap_or_default(),
                inputs: inputs.clone(),
                output_schema: node.output.as_ref().map(|o| o.schema.clone()),
            };

            let t = self.tick();
            self.store.set_state(id, NodeState::Running, t)?;

            if node.kind == NodeKind::Agent {
                self.model_calls += 1;
            }

            let outcome = match self.runner.run(&req) {
                Ok(o) => o,
                Err(e) => NodeOutcome::Failed {
                    error: e.to_string(),
                },
            };

            let error = match outcome {
                NodeOutcome::Succeeded { output } => match validate_output(&node, &output) {
                    Ok(()) => {
                        let t = self.tick();
                        self.store.record_output(id, &output, t)?;
                        let t = self.tick();
                        self.store.set_state(id, NodeState::Succeeded, t)?;
                        return Ok(());
                    }
                    Err(reason) => reason,
                },
                NodeOutcome::Failed { error } => error,
            };

            let t = self.tick();
            self.store.set_state(id, NodeState::Failed, t)?;

            // Strict ascent: retries, then repairs, then block. No
            // classifier decides which rung a failure "deserves" — that
            // would be a heuristic governing autonomous work, exactly the
            // implicit control flow this project exists to remove.
            let rung = if retries_left > 0 {
                retries_left -= 1;
                Rung::Retry
            } else if repairs_left > 0 {
                repairs_left -= 1;
                Rung::Repair
            } else {
                Rung::Block
            };

            let t = self.tick();
            self.store.record_attempt(id, rung.as_str(), &error, t)?;

            match rung {
                Rung::Retry => {
                    let t = self.tick();
                    self.store.set_state(id, NodeState::Recovering, t)?;
                }
                Rung::Repair => {
                    // The bound (`bound.rs`) budgets `2*repairs` model calls
                    // per agent node: one call to consult the recovery
                    // model, then one re-execution of the node. Only the
                    // re-execution was counted before this fix (at the top
                    // of the loop); count the consultation itself here so
                    // `RunReport.model_calls` and `Bound.agent_calls` meter
                    // the same thing. Repair budget is always 0 for
                    // non-agent nodes, so this rung is unreachable for them
                    // and the guard is just documentation-by-code.
                    if node.kind == NodeKind::Agent {
                        self.model_calls += 1;
                    }
                    match self.repairer.repair(&node, &error) {
                        // A repair that breaks the contract is not a repair
                        // — refuse it and block rather than let the graph
                        // silently mutate.
                        Some(revised) if contract_preserved(&original, &revised) => {
                            node = revised;
                            let t = self.tick();
                            self.store.set_state(id, NodeState::Recovering, t)?;
                        }
                        _ => {
                            let t = self.tick();
                            self.store.set_state(id, NodeState::Blocked, t)?;
                            return Ok(());
                        }
                    }
                }
                Rung::Block => {
                    let t = self.tick();
                    self.store.set_state(id, NodeState::Blocked, t)?;
                    return Ok(());
                }
            }
        }
    }

    fn report(&self) -> Result<RunReport, SghError> {
        let mut r = RunReport {
            model_calls: self.model_calls,
            ..Default::default()
        };
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
    let Some(spec) = &node.output else {
        return Ok(());
    };
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
