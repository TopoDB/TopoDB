use crate::schema::Node;

/// Rungs of the escalation ladder, climbed in strict order. There is
/// deliberately no classifier deciding which rung a failure "deserves": a
/// heuristic governing how much autonomous work happens is exactly the
/// implicit control flow this project exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    Retry,
    Repair,
    Block,
}

impl Rung {
    pub fn as_str(self) -> &'static str {
        match self {
            Rung::Retry => "retry",
            Rung::Repair => "repair",
            Rung::Block => "block",
        }
    }
}

/// Produces a revised node after a failure. Returning `None` declines to
/// repair, which sends the ladder straight to `Block`.
pub trait Repairer: Send + Sync {
    fn repair(&self, node: &Node, error: &str) -> Option<Node>;
}

/// The default: never repairs. Used when no model-backed repairer is wired in,
/// so tests and command-only graphs behave predictably.
pub struct NoopRepairer;

impl Repairer for NoopRepairer {
    fn repair(&self, _node: &Node, _error: &str) -> Option<Node> {
        None
    }
}

/// REPAIR may change how a node does its work, never its contract.
///
/// If a repair could widen a schema or add a dependency it would *be* a
/// replan: the frozen graph would have silently mutated and every termination
/// guarantee would evaporate. Enforced here rather than trusted to the
/// recovery prompt.
pub fn contract_preserved(original: &Node, repaired: &Node) -> bool {
    original.id == repaired.id
        && original.kind == repaired.kind
        && original.needs == repaired.needs
        && original.output == repaired.output
        && original.budget == repaired.budget
}
