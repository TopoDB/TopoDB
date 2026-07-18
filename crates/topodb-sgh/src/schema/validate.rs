use std::collections::{HashMap, HashSet};

use super::{Graph, NodeKind};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ValidationError {
    #[error("graph contains a cycle involving: {nodes:?}")]
    Cycle { nodes: Vec<String> },
    #[error("duplicate node id: {0}")]
    DuplicateId(String),
    #[error("node {node} depends on unknown node {missing}")]
    DanglingNeed { node: String, missing: String },
    #[error("agent node {0} has no prompt")]
    MissingPrompt(String),
    #[error("command node {0} has no run")]
    MissingRun(String),
    #[error("node {node} has a malformed output schema: {reason}")]
    InvalidSchema { node: String, reason: String },
}

/// A graph that has passed validation. Only a `Validated` may be executed.
#[derive(Debug, Clone)]
pub struct Validated {
    pub graph: Graph,
    /// Node ids in a deterministic topological order.
    pub topo_order: Vec<String>,
}

impl Validated {
    pub fn topo_index(&self, id: &str) -> Option<usize> {
        self.topo_order.iter().position(|n| n == id)
    }
}

pub fn validate(graph: &Graph) -> Result<Validated, Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Duplicate ids.
    let mut seen = HashSet::new();
    for n in &graph.nodes {
        if !seen.insert(n.id.as_str()) {
            errors.push(ValidationError::DuplicateId(n.id.clone()));
        }
    }

    // Per-node well-formedness.
    for n in &graph.nodes {
        match n.kind {
            NodeKind::Agent if n.prompt.is_none() => {
                errors.push(ValidationError::MissingPrompt(n.id.clone()))
            }
            NodeKind::Command if n.run.is_none() => {
                errors.push(ValidationError::MissingRun(n.id.clone()))
            }
            _ => {}
        }
        if let Some(spec) = &n.output {
            if let Err(e) = jsonschema::JSONSchema::compile(&spec.schema) {
                errors.push(ValidationError::InvalidSchema {
                    node: n.id.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }

    // Dangling dependencies.
    let ids: HashSet<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
    for n in &graph.nodes {
        for need in &n.needs {
            if !ids.contains(need.as_str()) {
                errors.push(ValidationError::DanglingNeed {
                    node: n.id.clone(),
                    missing: need.clone(),
                });
            }
        }
    }

    // Kahn's algorithm. Ties broken by declaration order for determinism.
    //
    // Indegree counts *distinct* existing dependencies per node, not raw
    // `needs` occurrences: a duplicated `needs` entry (e.g. `needs: [a, a]`)
    // is accepted as equivalent to declaring the dependency once — it is
    // redundant, not a structural error, and doesn't deserve its own
    // diagnostic. The removal side below (`other.needs.iter().any(...)`)
    // already only ever decrements once per dependent node regardless of how
    // many times a `need` is repeated, so incrementing once per occurrence
    // here (as before) left the counter permanently off by the duplicate
    // count and a valid graph was misreported as `Cycle`.
    let mut indegree: HashMap<&str, usize> =
        graph.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for n in &graph.nodes {
        let mut distinct: HashSet<&str> = HashSet::new();
        for need in &n.needs {
            if ids.contains(need.as_str()) {
                distinct.insert(need.as_str());
            }
        }
        *indegree.get_mut(n.id.as_str()).unwrap() += distinct.len();
    }

    let mut order: Vec<String> = Vec::with_capacity(graph.nodes.len());
    loop {
        let next = graph
            .nodes
            .iter()
            .find(|n| indegree.get(n.id.as_str()) == Some(&0));
        let Some(node) = next else { break };
        let id = node.id.clone();
        indegree.remove(id.as_str());
        for other in &graph.nodes {
            if other.needs.iter().any(|d| d == &id) {
                if let Some(d) = indegree.get_mut(other.id.as_str()) {
                    *d -= 1;
                }
            }
        }
        order.push(id);
    }

    if !indegree.is_empty() {
        let mut nodes: Vec<String> = indegree.keys().map(|s| s.to_string()).collect();
        nodes.sort();
        errors.push(ValidationError::Cycle { nodes });
    }

    if errors.is_empty() {
        Ok(Validated {
            graph: graph.clone(),
            topo_order: order,
        })
    } else {
        Err(errors)
    }
}
