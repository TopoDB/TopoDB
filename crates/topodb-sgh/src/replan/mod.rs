use crate::executor::RunReport;
use crate::planner::{PlanRequest, Planner, PlannerError};
use crate::schema::validate::Validated;
use crate::schema::{Graph, NodeKind};
use crate::store::run::RunStore;
use crate::store::SghError;

/// Prompts and `run` commands are truncated to this many characters when
/// rendered into a replan goal, so a multi-paragraph agent prompt doesn't
/// dominate the text handed to the planner.
const DESCRIPTION_TRUNCATE_LEN: usize = 200;

/// Why a run halted, assembled from persisted state rather than from
/// in-memory leftovers, so a proposal can be made from a stored run.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureContext {
    pub blocked: Vec<String>,
    pub skipped: Vec<String>,
    /// (node id, rung, error) for every recorded attempt on a blocked node.
    pub attempts: Vec<(String, String, String)>,
    /// node id -> short description of what the node was trying to do
    /// (its kind plus its prompt or run command), for every blocked node.
    pub descriptions: std::collections::BTreeMap<String, String>,
}

fn truncate(s: &str) -> String {
    if s.chars().count() <= DESCRIPTION_TRUNCATE_LEN {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(DESCRIPTION_TRUNCATE_LEN).collect();
        format!("{truncated}...")
    }
}

fn describe_node(kind: NodeKind, prompt: Option<&str>, run: Option<&str>) -> String {
    let kind_str = match kind {
        NodeKind::Agent => "agent",
        NodeKind::Command => "command",
        NodeKind::Gate => "gate",
    };
    match (prompt, run) {
        (Some(p), _) => format!("{kind_str}: {}", truncate(p)),
        (None, Some(r)) => format!("{kind_str}: {}", truncate(r)),
        (None, None) => kind_str.to_string(),
    }
}

pub fn collect_failure_context(
    store: &RunStore,
    graph: &Validated,
    report: &RunReport,
) -> Result<FailureContext, SghError> {
    let mut attempts = Vec::new();
    for node in &report.blocked {
        for (rung, error) in store.attempts(node)? {
            attempts.push((node.clone(), rung, error));
        }
    }

    let mut descriptions = std::collections::BTreeMap::new();
    for node_id in &report.blocked {
        if let Some(node) = graph.graph.node(node_id) {
            descriptions.insert(
                node_id.clone(),
                describe_node(node.kind, node.prompt.as_deref(), node.run.as_deref()),
            );
        }
    }

    Ok(FailureContext {
        blocked: report.blocked.clone(),
        skipped: report.skipped.clone(),
        attempts,
        descriptions,
    })
}

/// Restate the goal for a replanning pass.
///
/// The planner is told what was already tried and why it failed, and is
/// explicitly asked for a different approach — a proposal that reproduces
/// the failed graph would burn another approval cycle for nothing.
pub fn build_replan_goal(original_goal: &str, ctx: &FailureContext) -> String {
    let mut g = String::new();
    g.push_str(original_goal);
    g.push_str("\n\nA previous attempt at this goal halted and must be replaced with a different approach.\n\n");

    g.push_str("Steps that failed:\n");
    for node in &ctx.blocked {
        match ctx.descriptions.get(node) {
            Some(desc) => g.push_str(&format!("- {node}: {desc}\n")),
            None => g.push_str(&format!("- {node}\n")),
        }
    }

    if !ctx.attempts.is_empty() {
        g.push_str("\nWhat went wrong:\n");
        for (node, rung, error) in &ctx.attempts {
            g.push_str(&format!("- {node} ({rung}): {error}\n"));
        }
    }

    if !ctx.skipped.is_empty() {
        g.push_str("\nSteps never reached because they depended on a failed step:\n");
        for node in &ctx.skipped {
            g.push_str(&format!("- {node}\n"));
        }
    }

    g.push_str(
        "\nProduce a different plan that avoids the failure above. Do not repeat the same \
         steps unchanged. If the goal cannot be achieved as stated, produce the smallest \
         graph that establishes why.\n",
    );

    g
}

/// Ask the planner for a successor graph. The returned graph is validated by
/// the planner's own contract; callers must still gate it on a human.
pub fn propose_revision(
    planner: &dyn Planner,
    original: &Validated,
    ctx: &FailureContext,
) -> Result<Graph, PlannerError> {
    let goal = build_replan_goal(&original.graph.goal, ctx);
    planner.plan(&PlanRequest { goal, context: None })
}
