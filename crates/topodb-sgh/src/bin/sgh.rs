use std::path::PathBuf;

use clap::{Parser, Subcommand};
use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::planner::claude::ClaudePlanner;
use topodb_sgh::planner::{PlanRequest, Planner};
use topodb_sgh::replan::{collect_failure_context, propose_revision};
use topodb_sgh::runner::claude::ClaudeCodeRunner;
use topodb_sgh::runner::command::ShellCommandRunner;
use topodb_sgh::schema::bound::worst_case;
use topodb_sgh::schema::validate::{validate, Validated};
use topodb_sgh::schema::{Graph, NodeKind};
use topodb_sgh::store::run::RunStore;

#[derive(Parser)]
#[command(name = "sgh", about = "Structured Graph Harness")]
struct Cli {
    #[arg(long, default_value = "sgh.redb")]
    db: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate a graph and print its worst-case bound.
    Validate { graph: PathBuf },
    /// Execute a graph after showing its bound.
    Run {
        graph: PathBuf,
        #[arg(long)]
        model: Option<String>,
        /// Skip the approval prompt.
        #[arg(long)]
        yes: bool,
        /// Seconds a single command node may run before it is killed.
        #[arg(long, default_value_t = 120)]
        command_timeout: u64,
        /// On a halted run, ask the planner for a revised graph.
        #[arg(long)]
        replan: bool,
        /// How many revisions may be proposed before giving up.
        #[arg(long, default_value_t = 1)]
        max_replans: u32,
    },
    /// Compile a goal into a graph.yaml and print its worst-case bound.
    Plan {
        /// What you want done, in plain language.
        goal: String,
        /// Where to write the graph. Defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Optional grounding: repo facts, constraints, prior findings.
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// How many times the planner may retry an invalid graph.
        #[arg(long, default_value_t = 3)]
        max_attempts: u32,
    },
}

/// Every command node's id and full `run:` string, in declaration order.
///
/// Displayed verbatim at the approval gate. Commands may be model-authored
/// once the planner lands, so the human seeing exactly what will execute is
/// the control — never summarize or truncate these.
fn command_preview(v: &Validated) -> Vec<String> {
    v.graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Command)
        .map(|n| format!("{}: {}", n.id, n.run.clone().unwrap_or_default()))
        .collect()
}

/// Announce a replanning attempt and the ceiling it counts against, so the
/// bound on autonomous work is visible rather than implicit.
fn replan_banner(attempt: u32, max: u32) -> String {
    format!("\n=== replan {attempt} of {max} ===")
}

/// The decision made after a run halts: succeed, give up, or propose another
/// revision. Pulled out of the run loop so the bounding logic — the
/// security-critical piece that caps executions and planner calls — can be
/// unit-tested directly instead of only hand-traced.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Success,
    Exhausted,
    /// Carries the new `replans_used` value (already incremented) so the
    /// caller does not duplicate the increment.
    Replan(u32),
}

fn next_step(blocked_empty: bool, replan: bool, replans_used: u32, max_replans: u32) -> Step {
    if blocked_empty {
        return Step::Success;
    }
    if !replan || replans_used >= max_replans {
        return Step::Exhausted;
    }
    Step::Replan(replans_used + 1)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Validate { graph } => {
            let src = std::fs::read_to_string(&graph)?;
            let g = Graph::from_yaml(&src)?;
            match validate(&g) {
                Ok(v) => {
                    println!("valid: {} node(s)", v.graph.nodes.len());
                    println!("{}", worst_case(&v));
                    let commands = command_preview(&v);
                    if !commands.is_empty() {
                        println!("command nodes ({}):", commands.len());
                        for line in &commands {
                            println!("  {line}");
                        }
                    }
                }
                Err(errors) => {
                    for e in &errors {
                        eprintln!("error: {e}");
                    }
                    std::process::exit(2);
                }
            }
        }
        Cmd::Run {
            graph,
            model,
            yes,
            command_timeout,
            replan,
            max_replans,
        } => {
            let src = std::fs::read_to_string(&graph)?;
            let g = Graph::from_yaml(&src)?;
            let v = match validate(&g) {
                Ok(v) => v,
                Err(errors) => {
                    for e in &errors {
                        eprintln!("error: {e}");
                    }
                    std::process::exit(2);
                }
            };

            let db = Db::open(&cli.db)?;
            let command_runner =
                ShellCommandRunner::new(std::time::Duration::from_secs(command_timeout));
            let runner = ClaudeCodeRunner::new(model.clone());

            let mut current = v;
            let mut replans_used = 0u32;

            loop {
                let bound = worst_case(&current);
                println!("Goal: {}", current.graph.goal);
                println!("Nodes: {}", current.graph.nodes.len());
                println!("Bound: {bound}");

                let commands = command_preview(&current);
                if !commands.is_empty() {
                    println!("\nCommands that will execute in a shell:");
                    for line in &commands {
                        println!("  {line}");
                    }
                }

                if !yes {
                    println!("\nProceed? [y/N]");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    if !line.trim().eq_ignore_ascii_case("y") {
                        println!("aborted");
                        return Ok(());
                    }
                }

                let run_id = ulid::Ulid::new().to_string();
                let now = 1;
                let store = RunStore::create(&db, &run_id, &current, now)?;
                let mut ex = Executor::new(store, current.clone(), &runner)
                    .with_command_runner(&command_runner);
                let report = ex.run(now + 1)?;

                println!("\nrun {run_id}");
                println!("  succeeded: {:?}", report.succeeded);
                println!("  blocked:   {:?}", report.blocked);
                println!("  skipped:   {:?}", report.skipped);
                println!(
                    "  model calls: {} (bound was {})",
                    report.model_calls, bound.agent_calls
                );
                println!(
                    "  command runs: {} (bound was {})",
                    report.command_runs, bound.command_runs
                );

                match next_step(report.blocked.is_empty(), replan, replans_used, max_replans) {
                    Step::Success => return Ok(()),
                    Step::Exhausted => {
                        if replan {
                            eprintln!(
                                "error: run halted and the replan budget of {max_replans} is exhausted"
                            );
                        }
                        std::process::exit(1);
                    }
                    Step::Replan(n) => {
                        replans_used = n;
                    }
                }
                println!("{}", replan_banner(replans_used, max_replans));

                let ctx = collect_failure_context(ex.store_ref(), &current, &report)?;
                let planner = ClaudePlanner::new(model.clone(), 3);
                let revised = match propose_revision(&planner, &current, &ctx) {
                    Ok(g) => g,
                    Err(e) => {
                        eprintln!("error: replan failed: {e}");
                        std::process::exit(1);
                    }
                };

                // Validate before persisting: an invalid revision must never
                // land in the run's revision history.
                let validated = match validate(&revised) {
                    Ok(v) => v,
                    Err(errors) => {
                        for e in &errors {
                            eprintln!("error: proposed revision is invalid: {e}");
                        }
                        std::process::exit(2);
                    }
                };

                let revised_yaml = serde_yaml::to_string(&revised)?;
                ex.store_ref().record_revision(
                    &revised_yaml,
                    &format!("blocked: {:?}", report.blocked),
                    now + 2,
                )?;

                current = validated;

                println!("proposed revision:\n{revised_yaml}");
                // Loop back: the revision re-enters the gate exactly like the
                // original graph. It is never executed without approval.
            }
        }
        Cmd::Plan {
            goal,
            out,
            context,
            model,
            max_attempts,
        } => {
            let planner = ClaudePlanner::new(model, max_attempts);
            let graph = match planner.plan(&PlanRequest { goal, context }) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }
            };

            // The planner only returns validated graphs, but re-validate so
            // the bound below is computed from a proof-carrying value rather
            // than a trusted one.
            let v = match validate(&graph) {
                Ok(v) => v,
                Err(errors) => {
                    for e in &errors {
                        eprintln!("error: {e}");
                    }
                    std::process::exit(2);
                }
            };

            let yaml = serde_yaml::to_string(&graph)?;
            match &out {
                Some(path) => {
                    std::fs::write(path, &yaml)?;
                    eprintln!("wrote {} ({} node(s))", path.display(), v.graph.nodes.len());
                    eprintln!("{}", worst_case(&v));
                    let commands = command_preview(&v);
                    if !commands.is_empty() {
                        eprintln!("command nodes ({}):", commands.len());
                        for line in &commands {
                            eprintln!("  {line}");
                        }
                    }
                    eprintln!("review it, then: sgh run {}", path.display());
                }
                None => print!("{yaml}"),
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use topodb_sgh::schema::validate::validate;
    use topodb_sgh::schema::Graph;

    fn validated(yaml: &str) -> topodb_sgh::schema::validate::Validated {
        validate(&Graph::from_yaml(yaml).expect("parses")).expect("valid")
    }

    #[test]
    fn command_preview_lists_every_command_with_its_full_run_string() {
        let v = validated(
            "version: 1\ngoal: g\nnodes:\n\
             - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
             - {id: b, kind: command, run: 'cargo build -p topodb', budget: {retries: 0, repairs: 0}}\n\
             - {id: c, kind: command, run: 'rm -rf ./tmp', budget: {retries: 0, repairs: 0}}\n",
        );
        let lines = command_preview(&v);
        assert_eq!(
            lines,
            vec![
                "b: cargo build -p topodb".to_string(),
                "c: rm -rf ./tmp".to_string(),
            ],
            "every command must be shown in full — the gate is the control on model-authored shell"
        );
    }

    #[test]
    fn command_preview_is_empty_without_command_nodes() {
        let v = validated(
            "version: 1\ngoal: g\nnodes:\n\
             - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n",
        );
        assert!(command_preview(&v).is_empty());
    }

    #[test]
    fn replan_banner_states_which_attempt_and_the_ceiling() {
        let b = replan_banner(1, 2);
        assert!(b.contains('1'), "current attempt must be shown");
        assert!(b.contains('2'), "the ceiling must be shown so the bound is visible");
    }

    #[test]
    fn next_step_succeeds_whenever_nothing_is_blocked_regardless_of_other_args() {
        assert_eq!(next_step(true, false, 0, 0), Step::Success);
        assert_eq!(next_step(true, true, 5, 1), Step::Success);
        assert_eq!(next_step(true, true, 0, 100), Step::Success);
    }

    #[test]
    fn next_step_exhausts_when_replan_flag_is_off_even_with_budget_remaining() {
        assert_eq!(next_step(false, false, 0, 5), Step::Exhausted);
    }

    #[test]
    fn next_step_replans_at_replans_used_one_below_max() {
        // max_replans = 3: the third replan (replans_used going 2 -> 3) is
        // still allowed.
        assert_eq!(next_step(false, true, 2, 3), Step::Replan(3));
    }

    #[test]
    fn next_step_exhausts_at_replans_used_equal_to_max() {
        // Once replans_used has reached max_replans, the budget is spent.
        assert_eq!(next_step(false, true, 3, 3), Step::Exhausted);
    }

    #[test]
    fn next_step_exhausts_immediately_when_max_replans_is_zero_no_planner_calls() {
        assert_eq!(next_step(false, true, 0, 0), Step::Exhausted);
    }
}
