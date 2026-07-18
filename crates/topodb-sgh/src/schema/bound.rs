use super::validate::Validated;
use super::NodeKind;

/// The worst-case cost of a graph, computable before the run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    /// Maximum model calls across the whole run.
    pub agent_calls: u64,
    /// Maximum shell command executions across the whole run.
    pub command_runs: u64,
}

/// An agent node costs `1 + retries + 2*repairs` model calls: each repair
/// consults the recovery model once, then re-executes the node once.
/// A command node costs `1 + retries` command executions: retries re-run the
/// command, but repairs are not counted (a shell command cannot be "repaired"
/// by consulting a model — only retried). Gate nodes cost nothing.
pub fn worst_case(v: &Validated) -> Bound {
    let mut agent_calls = 0u64;
    let mut command_runs = 0u64;

    for n in &v.graph.nodes {
        let retries = n.budget.retries as u64;
        let repairs = n.budget.repairs as u64;
        match n.kind {
            NodeKind::Agent => agent_calls += 1 + retries + 2 * repairs,
            NodeKind::Command => command_runs += 1 + retries,
            NodeKind::Gate => {}
        }
    }

    Bound {
        agent_calls,
        command_runs,
    }
}

impl std::fmt::Display for Bound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "at most {} model call(s) and {} command run(s) before this graph halts",
            self.agent_calls, self.command_runs
        )
    }
}
