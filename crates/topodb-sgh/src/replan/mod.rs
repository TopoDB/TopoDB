use crate::executor::RunReport;
use crate::planner::{PlanRequest, Planner, PlannerError};
use crate::schema::validate::Validated;
use crate::schema::Graph;
use crate::store::run::RunStore;
use crate::store::SghError;

/// Why a run halted, assembled from persisted state rather than from
/// in-memory leftovers, so a proposal can be made from a stored run.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureContext {
    pub blocked: Vec<String>,
    pub skipped: Vec<String>,
    /// (node id, rung, error) for every recorded attempt on a blocked node.
    pub attempts: Vec<(String, String, String)>,
}

pub fn collect_failure_context(
    store: &RunStore,
    report: &RunReport,
) -> Result<FailureContext, SghError> {
    let mut attempts = Vec::new();
    for node in &report.blocked {
        for (rung, error) in store.attempts(node)? {
            attempts.push((node.clone(), rung, error));
        }
    }

    Ok(FailureContext {
        blocked: report.blocked.clone(),
        skipped: report.skipped.clone(),
        attempts,
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
        g.push_str(&format!("- {node}\n"));
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
