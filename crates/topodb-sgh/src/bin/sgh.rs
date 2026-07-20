use std::path::PathBuf;

use clap::{Parser, Subcommand};
use topodb::Db;
use topodb_sgh::executor::{Executor, RunReport};
use topodb_sgh::planner::claude::ClaudePlanner;
use topodb_sgh::planner::{PlanRequest, Planner};
use topodb_sgh::replan::{collect_failure_context, propose_revision, FailureContext};
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
    ///
    /// Exit codes a script may rely on:
    ///   0 — the run completed with nothing blocked.
    ///   1 — the run is blocked by a real failure (or the replan budget was
    ///       exhausted while a real failure remained).
    ///   2 — the graph (or a proposed revision) failed schema validation.
    ///   3 — the run halted at an intentional checkpoint: every blocked node
    ///       was a `gate`, not a failure. No replan was attempted and no
    ///       replan budget was spent. Distinct from 0 because the run did not
    ///       finish, and distinct from 1 because nothing actually failed —
    ///       this lets `sgh run plan.yaml && next-step.sh` tell "stopped on
    ///       purpose" apart from "broke".
    Run {
        graph: PathBuf,
        #[arg(long)]
        model: Option<String>,
        /// Skip the approval prompt for the graph given on the command
        /// line. Does NOT cover replan revisions: once `--replan` is in
        /// play, a model can rewrite the graph's `run:` strings, and a
        /// human seeing that text before it executes is the only control —
        /// so every revision still prompts, even with `--yes`. Use
        /// `--yes-including-revisions` for the fully unattended case.
        #[arg(long)]
        yes: bool,
        /// Skip the approval prompt for EVERY graph, including replan
        /// revisions that a model has authored and this process has not
        /// shown you yet. This approves shell commands a model has not yet
        /// written. Implies `--yes`.
        #[arg(long)]
        yes_including_revisions: bool,
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

/// Whether the approval gate must prompt before this iteration's graph
/// executes.
///
/// `--yes` covers only the graph the operator supplied on the command line
/// (`is_revision == false`); a replan revision is authored by a model and
/// must always be shown to a human first, regardless of `--yes` — that is
/// the one control this project relies on for model-authored shell. Only
/// `--yes-including-revisions` (which implies `--yes`) skips the prompt for
/// a revision, and its help text says plainly that doing so approves shell
/// commands a model has not yet written.
fn needs_prompt(is_revision: bool, yes: bool, yes_including_revisions: bool) -> bool {
    if yes_including_revisions {
        return false;
    }
    if is_revision {
        return true;
    }
    !yes
}

/// Whether every node the run halted on is a gate rather than a real
/// failure. When true, the run stopped exactly as designed — a checkpoint
/// was reached, not a failure — and replanning must not be attempted: doing
/// so would spend replan budget on a run that didn't actually fail, and
/// would hand the planner a "failure" to avoid that was in fact intentional
/// (see `replan::build_replan_goal`'s gated-checkpoint section).
fn all_blocked_are_gates(ctx: &topodb_sgh::replan::FailureContext) -> bool {
    ctx.blocked.is_empty() && !ctx.gated.is_empty()
}

/// What a completed `sgh run` invocation should report to its caller,
/// decoupled from *how* — the process exit code — so the decision itself is
/// unit-testable.
///
/// A gate-only halt (`HaltedAtCheckpoint`) is deliberately distinct from both
/// `Completed` and `Blocked`: it must not consume replan budget or reach the
/// planner (see `all_blocked_are_gates`), but it also must not report success
/// — the run did not finish, and whatever the gate was guarding was not
/// reached. See the `Run` subcommand's doc comment for the exit-code
/// contract a script author can rely on.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Completed,
    HaltedAtCheckpoint,
    Blocked,
}

/// Classify a halted (or clean) run. A genuine failure always dominates: a
/// mixed set containing both a real failure and a gate is `Blocked`, not
/// `HaltedAtCheckpoint` — `all_blocked_are_gates` only returns true when
/// `ctx.blocked` (the non-gate failures) is empty.
fn outcome_of(report: &RunReport, ctx: &FailureContext) -> Outcome {
    if report.blocked.is_empty() {
        Outcome::Completed
    } else if all_blocked_are_gates(ctx) {
        Outcome::HaltedAtCheckpoint
    } else {
        Outcome::Blocked
    }
}

/// The process exit code for each `Outcome`. See the `Run` subcommand's doc
/// comment for the documented, script-facing contract this implements.
fn exit_code(outcome: &Outcome) -> i32 {
    match outcome {
        Outcome::Completed => 0,
        Outcome::HaltedAtCheckpoint => 3,
        Outcome::Blocked => 1,
    }
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
            yes_including_revisions,
            command_timeout,
            replan,
            max_replans,
        } => {
            // `--yes-including-revisions` implies `--yes` for anything else
            // in this command that reads `yes` (there is nothing else today,
            // but keeping the invariant explicit here means a future reader
            // of `yes` doesn't have to know about the other flag).
            let yes = yes || yes_including_revisions;
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
            // False for the graph supplied on the command line; set true
            // once a revision replaces `current` at the bottom of the loop.
            // Drives `needs_prompt` — the gate `--yes` alone must never
            // skip.
            let mut is_revision = false;

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

                if needs_prompt(is_revision, yes, yes_including_revisions) {
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

                if report.blocked.is_empty() {
                    return Ok(());
                }

                // Compute failure context before touching the replan budget:
                // a gate-only halt must not consume it (see
                // `all_blocked_are_gates`), so this has to happen before
                // `next_step` would otherwise increment `replans_used`.
                let ctx = collect_failure_context(ex.store_ref(), &current, &report)?;

                match outcome_of(&report, &ctx) {
                    Outcome::Completed => {
                        unreachable!("report.blocked was already checked non-empty above")
                    }
                    Outcome::HaltedAtCheckpoint => {
                        println!(
                            "\nrun halted at an intentional checkpoint, not a failure: {:?}",
                            ctx.gated
                        );
                        println!(
                            "no replan attempted — this is the run stopping as designed; the \
                             replan budget was not spent."
                        );
                        std::process::exit(exit_code(&Outcome::HaltedAtCheckpoint));
                    }
                    Outcome::Blocked => {
                        eprintln!("\nrun blocked by a failure: {:?}", ctx.blocked);
                    }
                }

                match next_step(false, replan, replans_used, max_replans) {
                    Step::Success => {
                        unreachable!("report.blocked was already checked non-empty above")
                    }
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

                // Stamp strictly after every write the run itself made
                // (`ex.clock()` is the executor's logical clock after
                // `run()` returns), not `now + 2` — the executor's clock
                // ticks on every state write and can reach well past that by
                // the time the run halts, so a fixed `now + 2` recorded the
                // revision as existing *during* the run that produced it.
                let revised_yaml = serde_yaml::to_string(&revised)?;
                ex.store_ref().record_revision(
                    &revised_yaml,
                    &format!("blocked: {:?}", report.blocked),
                    ex.clock() + 1,
                )?;

                current = validated;
                is_revision = true;

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
        assert!(
            b.contains('2'),
            "the ceiling must be shown so the bound is visible"
        );
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

    // needs_prompt: the gate-decision truth table. `--yes` covers only the
    // original graph; a revision always prompts unless
    // `--yes-including-revisions` is set.
    #[test]
    fn needs_prompt_original_graph_with_yes_skips_the_prompt() {
        assert!(!needs_prompt(false, true, false));
    }

    #[test]
    fn needs_prompt_revision_with_yes_still_prompts() {
        assert!(needs_prompt(true, true, false));
    }

    #[test]
    fn needs_prompt_revision_with_yes_including_revisions_skips_the_prompt() {
        assert!(!needs_prompt(true, false, true));
        assert!(!needs_prompt(true, true, true));
    }

    #[test]
    fn needs_prompt_with_no_flags_always_prompts() {
        assert!(
            needs_prompt(false, false, false),
            "original graph, no flags"
        );
        assert!(needs_prompt(true, false, false), "revision, no flags");
    }

    #[test]
    fn needs_prompt_original_graph_with_yes_including_revisions_skips_the_prompt() {
        // yes_including_revisions implies yes, so the original graph is
        // covered too.
        assert!(!needs_prompt(false, false, true));
    }

    // all_blocked_are_gates: whether a halted run stopped only at intentional
    // checkpoints, in which case replanning must not be attempted at all.
    fn ctx(blocked: Vec<&str>, gated: Vec<&str>) -> topodb_sgh::replan::FailureContext {
        topodb_sgh::replan::FailureContext {
            blocked: blocked.into_iter().map(String::from).collect(),
            gated: gated.into_iter().map(String::from).collect(),
            skipped: vec![],
            attempts: vec![],
            descriptions: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn all_blocked_are_gates_true_when_only_gates_blocked() {
        assert!(all_blocked_are_gates(&ctx(vec![], vec!["checkpoint"])));
    }

    #[test]
    fn all_blocked_are_gates_false_when_a_real_failure_is_present() {
        assert!(!all_blocked_are_gates(&ctx(vec!["a"], vec!["checkpoint"])));
        assert!(!all_blocked_are_gates(&ctx(vec!["a"], vec![])));
    }

    #[test]
    fn all_blocked_are_gates_false_when_nothing_blocked() {
        // Not the case this predicate is meant to answer (the caller already
        // returns success before reaching it), but it must not report a
        // gate-only halt when there was no halt at all.
        assert!(!all_blocked_are_gates(&ctx(vec![], vec![])));
    }

    // outcome_of / exit_code: this is the regression pin. A previous fix
    // wave made a gate-only halt `return Ok(())` (exit 0) unconditionally,
    // which turned "the run stopped at a checkpoint" into a success signal
    // for scripts chained with `&&`. These tests pin that a gate-only halt
    // is neither `Completed` (0) nor `Blocked` (1), and that a genuine
    // failure always dominates a mixed set.
    fn report(blocked: Vec<&str>) -> RunReport {
        RunReport {
            succeeded: vec![],
            blocked: blocked.into_iter().map(String::from).collect(),
            skipped: vec![],
            model_calls: 0,
            command_runs: 0,
        }
    }

    #[test]
    fn outcome_of_clean_run_is_completed_exit_0() {
        let outcome = outcome_of(&report(vec![]), &ctx(vec![], vec![]));
        assert_eq!(outcome, Outcome::Completed);
        assert_eq!(exit_code(&outcome), 0);
    }

    #[test]
    fn outcome_of_gate_only_halt_is_halted_at_checkpoint_not_success() {
        let outcome = outcome_of(&report(vec!["gate1"]), &ctx(vec![], vec!["gate1"]));
        assert_eq!(outcome, Outcome::HaltedAtCheckpoint);
        let code = exit_code(&outcome);
        assert_ne!(code, 0, "a checkpoint halt must never look like success");
        assert_ne!(
            code, 1,
            "a checkpoint halt must be distinguishable from a real failure"
        );
        assert_eq!(code, 3);
    }

    #[test]
    fn outcome_of_real_failure_is_blocked_exit_1() {
        let outcome = outcome_of(&report(vec!["a"]), &ctx(vec!["a"], vec![]));
        assert_eq!(outcome, Outcome::Blocked);
        assert_eq!(exit_code(&outcome), 1);
    }

    #[test]
    fn outcome_of_mixed_failure_and_gate_is_blocked_because_failure_dominates() {
        let outcome = outcome_of(&report(vec!["a", "gate1"]), &ctx(vec!["a"], vec!["gate1"]));
        assert_eq!(
            outcome,
            Outcome::Blocked,
            "a genuine failure must dominate even when a gate also blocked"
        );
        assert_eq!(exit_code(&outcome), 1);
    }
}
