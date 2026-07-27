use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use topodb::Db;
use topodb_sgh::executor::{Executor, RunReport};
#[cfg(feature = "claude-code")]
use topodb_sgh::planner::claude::claude_planner_with_timeout;
use topodb_sgh::planner::{PlanRequest, Planner};
use topodb_sgh::replan::{collect_failure_context, propose_revision, FailureContext};
#[cfg(feature = "claude-code")]
use topodb_sgh::runner::claude::ClaudeCodeRunner;
use topodb_sgh::runner::command::ShellCommandRunner;
use topodb_sgh::schema::bound::worst_case;
use topodb_sgh::schema::validate::{validate, Validated};
use topodb_sgh::schema::{Graph, NodeKind, TOPODB_TOOL};
use topodb_sgh::store::run::RunStore;

/// Which backend executes agent nodes and (for `plan`) the planner itself.
///
/// `ClaudeCode` spawns the local `claude` CLI (requires the `claude-code`
/// feature); `Anthropic`/`Openai` talk to an HTTP chat API directly
/// (requires the `anthropic`/`openai` feature respectively).
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Provider {
    /// Spawn the local `claude` CLI (requires the claude-code feature).
    ClaudeCode,
    /// Anthropic Messages API over HTTPS (ANTHROPIC_API_KEY).
    Anthropic,
    /// Any OpenAI-compatible chat/completions endpoint (OPENAI_API_KEY / --base-url).
    Openai,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Provider::ClaudeCode => "claude-code",
            Provider::Anthropic => "anthropic",
            Provider::Openai => "openai",
        };
        write!(f, "{s}")
    }
}

/// Validate the `--provider`/`--base-url`/`--agent-bash` combination shared by
/// `run` and `plan`. Must run before any file IO — these are flag rails, not
/// graph-dependent checks. Exits the process with code 2 on violation.
fn validate_provider_flags(provider: Provider, base_url: &Option<String>, agent_bash_used: bool) {
    if base_url.is_some() && provider != Provider::Openai {
        eprintln!("error: --base-url applies only to --provider openai");
        std::process::exit(2);
    }
    if agent_bash_used && provider != Provider::ClaudeCode {
        eprintln!(
            "error: --agent-bash applies only to --provider claude-code (HTTP providers execute no shell)"
        );
        std::process::exit(2);
    }
}

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
    Validate {
        graph: PathBuf,
        /// Supply agent nodes with the TopoDB MCP server: the full server
        /// command (binary + args), e.g.
        /// "--agent-mcp '/abs/topodb-mcp --db /abs/memory.redb --scope <ulid>'".
        /// Whitespace-split with no shell; same character rail as --agent-bash
        /// (prefer an absolute binary path; paths with spaces unsupported).
        /// Only agent nodes declaring `tools: [topodb]` see the server
        /// (additive, echoed at the approval gate). Nodes opting in without
        /// this flag are a validate error.
        #[arg(long = "agent-mcp")]
        agent_mcp: Option<String>,
    },
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
        /// Which backend executes agent nodes. Defaults to the local `claude`
        /// CLI (requires the claude-code feature).
        #[arg(long, value_enum, default_value = "claude-code")]
        provider: Provider,
        /// Base URL override, valid only with --provider openai (points at
        /// any OpenAI-compatible endpoint: vLLM, Ollama, etc).
        #[arg(long)]
        base_url: Option<String>,
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
        /// Grant agent nodes permission to run shell commands starting with this
        /// prefix (repeatable). Additive on top of Read/Write/Edit. Echoed at the
        /// approval gate. Grant the narrowest binary (e.g. 'topodb'), never a shell.
        /// The grant must textually prefix-match the exact command your prompts
        /// issue — if a prompt invokes an absolute path like /abs/path/topodb, pass
        /// --agent-bash /abs/path/topodb, not --agent-bash topodb.
        #[arg(long = "agent-bash")]
        agent_bash: Vec<String>,
        /// Supply agent nodes with the TopoDB MCP server: the full server
        /// command (binary + args), e.g.
        /// "--agent-mcp '/abs/topodb-mcp --db /abs/memory.redb --scope <ulid>'".
        /// Whitespace-split with no shell; same character rail as --agent-bash
        /// (prefer an absolute binary path; paths with spaces unsupported).
        /// Only agent nodes declaring `tools: [topodb]` see the server
        /// (additive, echoed at the approval gate). Nodes opting in without
        /// this flag are a validate error.
        #[arg(long = "agent-mcp")]
        agent_mcp: Option<String>,
        /// Seconds a single command node may run before it is killed.
        #[arg(long, default_value_t = 120)]
        command_timeout: u64,
        /// Seconds a single agent-node model call (the whole node, not one
        /// HTTP request) may run before it is treated as failed. Applies to
        /// both the claude-code runner (subprocess deadline) and the HTTP
        /// runners (whole tool-loop deadline).
        #[arg(long, default_value_t = 600)]
        agent_timeout: u64,
        /// On a halted run, ask the planner for a revised graph.
        #[arg(long)]
        replan: bool,
        /// How many revisions may be proposed before giving up.
        #[arg(long, default_value_t = 1)]
        max_replans: u32,
        /// Maximum number of ready nodes executed concurrently. 1 forces
        /// strictly sequential execution.
        #[arg(long, default_value_t = 4)]
        max_inflight: usize,
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
        /// Which backend compiles the goal into a graph. Defaults to the
        /// local `claude` CLI (requires the claude-code feature). HTTP
        /// providers are wired in a later task (ApiBackend).
        #[arg(long, value_enum, default_value = "claude-code")]
        provider: Provider,
        /// Base URL override, valid only with --provider openai.
        #[arg(long)]
        base_url: Option<String>,
        /// How many times the planner may retry an invalid graph.
        #[arg(long, default_value_t = 3)]
        max_attempts: u32,
        /// Seconds a single planner call may run before it is treated as
        /// failed.
        #[arg(long, default_value_t = 600)]
        agent_timeout: u64,
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

/// Agent bash grant prefixes for display at the approval gate.
///
/// Empty input yields empty output. Formatting is applied at the call site.
fn grants_preview(grants: &[String]) -> Vec<String> {
    grants.to_vec()
}

/// Agent nodes opted into the TopoDB MCP surface, in declaration order.
/// Recomputed per loop iteration so replan revisions are covered.
fn mcp_nodes(v: &Validated) -> Vec<String> {
    v.graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Agent && n.tools.iter().any(|t| t == TOPODB_TOOL))
        .map(|n| n.id.clone())
        .collect()
}

/// The gate's MCP echo block, one line per entry (the caller prints the
/// separating blank line). A graph with no opted-in nodes still gets the
/// header plus the inert-flag note — the flag WAS passed, and silence would
/// hide that it does nothing for this graph. Names the claude-side surface
/// `mcp__topodb`, matching how claude itself sees the server.
#[cfg(feature = "claude-code")]
fn mcp_gate_lines(v: &Validated, server_cmd: &str) -> Vec<String> {
    mcp_gate_lines_named(v, server_cmd, "mcp__topodb")
}

/// Provider-neutral variant for HTTP providers: the namespaced tool surface
/// is `topodb__*`, not claude's `mcp__topodb` — the bridge exposes tools
/// prefixed `topodb__<name>` (see `mcp_bridge::TOOL_NAMESPACE`).
fn http_mcp_gate_lines(v: &Validated, server_cmd: &str) -> Vec<String> {
    mcp_gate_lines_named(v, server_cmd, "topodb__*")
}

/// Shared formatting for both the claude and HTTP gate echoes; only the
/// named tool surface differs.
fn mcp_gate_lines_named(v: &Validated, server_cmd: &str, surface: &str) -> Vec<String> {
    let nodes = mcp_nodes(v);
    let mut lines = vec![
        "Agent-node MCP server (additive; agent prompts are ungated):".to_string(),
        format!("  {}", server_cmd),
    ];
    if nodes.is_empty() {
        lines.push(format!(
            "  tools: {surface} -> no node opts in (flag is inert for this graph)"
        ));
    } else {
        lines.push(format!("  tools: {surface} -> nodes: {}", nodes.join(", ")));
    }
    lines
}

/// The pairing rule: a graph whose nodes opt into `tools: [topodb]` cannot
/// run (or validate clean) without `--agent-mcp` supplying the server —
/// failing here, at the gate, beats failing mid-run inside a model call.
fn mcp_pairing_error(v: &Validated, agent_mcp: Option<&str>) -> Option<String> {
    let nodes = mcp_nodes(v);
    if nodes.is_empty() || agent_mcp.is_some() {
        return None;
    }
    Some(format!(
        "node(s) {} declare tools: [topodb] but no --agent-mcp was given — \
         pass --agent-mcp '<topodb-mcp binary + args>' or remove the opt-in",
        nodes.join(", ")
    ))
}

/// Write the mcp-config JSON claude consumes (`--mcp-config`). One file per
/// run in the OS temp dir; content is identical for every node. Left behind
/// after the run — it is temp-dir housekeeping, and deleting it while claude
/// subprocesses might still read it would be a race.
#[cfg(feature = "claude-code")]
fn write_mcp_config(server_argv: &[String]) -> std::io::Result<std::path::PathBuf> {
    let body = serde_json::json!({
        "mcpServers": {
            "topodb": { "command": server_argv[0], "args": &server_argv[1..] }
        }
    });
    let path = std::env::temp_dir().join(format!("sgh-mcp-{}.json", ulid::Ulid::new()));
    std::fs::write(&path, serde_json::to_string_pretty(&body)?)?;
    Ok(path)
}

/// Agent nodes with no declared `output.schema`.
///
/// The executor validates a node's output only when a schema is declared, so
/// these nodes are accepted whatever they return. Their success is
/// self-reported: the runner can now tell that a tool was denied, but not that
/// an agent which was granted everything actually did the work rather than
/// reporting that it had. Surfaced at the approval gate so "succeeded" on such
/// a node is read as a claim, not as evidence.
fn unconstrained_agents(v: &Validated) -> Vec<String> {
    v.graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Agent && n.output.is_none())
        .map(|n| n.id.clone())
        .collect()
}

/// Report which agent nodes report their own success unchecked. Printed at
/// the same moment as the command preview: both are things a person should
/// weigh before approving a run.
fn print_unconstrained(v: &Validated) {
    let ids = unconstrained_agents(v);
    if ids.is_empty() {
        return;
    }
    println!("self-reported nodes ({}): {}", ids.len(), ids.join(", "));
    println!("  these declare no output schema, so whatever they return is accepted —");
    println!("  their `succeeded` is a claim, not evidence. Check the result yourself.");
}

/// Flatten a possibly multi-line failure reason to a single readable row.
/// Newlines and runs of whitespace collapse to single spaces; anything past
/// 200 chars is elided. The full text lives in the run store — this is the
/// at-a-glance version printed under the blocked list.
fn one_line(s: &str) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 200 {
        return flat;
    }
    let head: String = flat.chars().take(200).collect();
    format!("{head}…")
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

/// Every `BuiltRunner` variant behind one `&dyn AgentRunner` so the executor
/// loop below never has to branch on provider again once construction is
/// done. `Http` and `Claude` are each only compiled with their feature —
/// with neither `claude-code` nor `http` this enum is uninhabited, which is
/// fine: every construction site is guarded so it is never reached in that
/// build (every provider is rejected at the flag-rail stage instead).
enum BuiltRunner {
    #[cfg(feature = "claude-code")]
    Claude(ClaudeCodeRunner),
    #[cfg(feature = "http")]
    Http(Box<topodb_sgh::runner::http::HttpChatRunner>),
}

impl BuiltRunner {
    fn as_agent_runner(&self) -> &dyn topodb_sgh::runner::AgentRunner {
        match self {
            #[cfg(feature = "claude-code")]
            BuiltRunner::Claude(r) => r,
            #[cfg(feature = "http")]
            BuiltRunner::Http(r) => r.as_ref(),
            // With neither feature compiled, `BuiltRunner` has no variants
            // and no value of it can ever exist — every provider is
            // rejected at the flag-rail stage before construction. `&T` is
            // always inhabited even when `T` is not, so the match still
            // needs this arm to be exhaustive.
            #[cfg(not(any(feature = "claude-code", feature = "http")))]
            _ => unreachable!("BuiltRunner is uninhabited without claude-code or http"),
        }
    }
}

/// Build the chat-provider client for an HTTP provider. Never called with
/// `Provider::ClaudeCode`. `from_env` only reads environment variables (and
/// `--base-url`) — no network call and no subprocess — so this is safe to
/// run before the approval gate below; only the MCP bridge, which does spawn
/// a process, waits until after approval.
fn build_http_provider(
    provider: Provider,
    model: Option<String>,
    base_url: Option<String>,
) -> Box<dyn topodb_sgh::provider::ChatProvider> {
    match provider {
        Provider::ClaudeCode => {
            unreachable!("build_http_provider is only called for HTTP providers")
        }
        Provider::Anthropic => {
            #[cfg(feature = "anthropic")]
            {
                match topodb_sgh::provider::anthropic::AnthropicProvider::from_env(model) {
                    Ok(p) => Box::new(p),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }
                }
            }
            #[cfg(not(feature = "anthropic"))]
            {
                eprintln!("error: this sgh was built without the anthropic feature");
                std::process::exit(2);
            }
        }
        Provider::Openai => {
            #[cfg(feature = "openai")]
            {
                match topodb_sgh::provider::openai::OpenAiProvider::from_env(model, base_url) {
                    Ok(p) => Box::new(p),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }
                }
            }
            #[cfg(not(feature = "openai"))]
            {
                eprintln!("error: this sgh was built without the openai feature");
                std::process::exit(2);
            }
        }
    }
}

/// Build the planner used for a `--replan` revision. `ClaudeCode` reuses the
/// existing `claude_planner`; the HTTP providers build a fresh
/// `ApiBackend` — fresh because the runner's own provider client
/// (`pending_http_provider`) was already consumed building the executor's
/// `AgentRunner`, so planning needs its own client, not a shared one.
/// `from_env` is cheap (env vars only, no IO), so building it again here
/// costs nothing.
fn build_replan_planner(
    provider: Provider,
    model: Option<String>,
    base_url: Option<String>,
    agent_timeout: Duration,
) -> topodb_sgh::planner::BoundedPlanner {
    match provider {
        Provider::ClaudeCode => {
            #[cfg(feature = "claude-code")]
            {
                claude_planner_with_timeout(model, 3, agent_timeout)
            }
            #[cfg(not(feature = "claude-code"))]
            {
                unreachable!(
                    "claude-code provider without the feature already rejected at the flag-rail stage"
                )
            }
        }
        Provider::Anthropic | Provider::Openai => {
            #[cfg(feature = "http")]
            {
                let provider_client = build_http_provider(provider, model.clone(), base_url);
                let backend = topodb_sgh::planner::api::ApiBackend::new(provider_client, model)
                    .with_timeout(agent_timeout);
                topodb_sgh::planner::BoundedPlanner::with_backend(Box::new(backend), 3)
            }
            #[cfg(not(feature = "http"))]
            {
                unreachable!(
                    "http provider without the feature already rejected building the runner's own provider client above"
                )
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Validate { graph, agent_mcp } => {
            let src = std::fs::read_to_string(&graph)?;
            let g = Graph::from_yaml(&src)?;
            match validate(&g) {
                Ok(v) => {
                    if let Some(cmd) = &agent_mcp {
                        if let Err(e) = topodb_sgh::runner::rails::validate_mcp_server_command(cmd)
                        {
                            eprintln!("error: {e}");
                            std::process::exit(2);
                        }
                    }
                    if let Some(err) = mcp_pairing_error(&v, agent_mcp.as_deref()) {
                        eprintln!("error: {err}");
                        std::process::exit(2);
                    }
                    println!("valid: {} node(s)", v.graph.nodes.len());
                    println!("{}", worst_case(&v));
                    let commands = command_preview(&v);
                    if !commands.is_empty() {
                        println!("command nodes ({}):", commands.len());
                        for line in &commands {
                            println!("  {line}");
                        }
                    }
                    print_unconstrained(&v);
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
            agent_bash,
            agent_mcp,
            command_timeout,
            agent_timeout,
            replan,
            max_replans,
            provider,
            base_url,
            max_inflight,
        } => {
            let code = run_cmd(
                &cli.db,
                graph,
                model,
                yes,
                yes_including_revisions,
                agent_bash,
                agent_mcp,
                command_timeout,
                agent_timeout,
                replan,
                max_replans,
                provider,
                base_url,
                max_inflight,
            )?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::Plan {
            goal,
            out,
            context,
            model,
            provider,
            base_url,
            max_attempts,
            agent_timeout,
        } => plan_cmd(
            goal,
            out,
            context,
            model,
            provider,
            base_url,
            max_attempts,
            agent_timeout,
        )?,
    }

    Ok(())
}

/// The `run` subcommand's body, factored out of `main` so every exit code is
/// a plain `Result` value instead of a `std::process::exit` call. This
/// matters specifically because `std::process::exit` skips destructors: an
/// HTTP provider's `McpBridge` (a live child process) is owned by a local
/// here, and only a normal `return` — not `process::exit` — runs `Drop`
/// (kill+wait) on it before the process actually terminates. `main` is the
/// only place still allowed to call `process::exit`, and only after this
/// function has already returned (so everything local to it, runner and
/// bridge included, has already dropped).
#[allow(clippy::too_many_arguments)]
fn run_cmd(
    db_path: &std::path::Path,
    graph: PathBuf,
    model: Option<String>,
    yes: bool,
    yes_including_revisions: bool,
    agent_bash: Vec<String>,
    agent_mcp: Option<String>,
    command_timeout: u64,
    agent_timeout: u64,
    replan: bool,
    max_replans: u32,
    provider: Provider,
    base_url: Option<String>,
    max_inflight: usize,
) -> Result<i32, Box<dyn std::error::Error>> {
    let agent_timeout = Duration::from_secs(agent_timeout);
    let cancel = topodb_sgh::runner::cancel::CancelToken::new();
    let cancel_for_handler = cancel.clone();
    ctrlc::set_handler(move || cancel_for_handler.cancel())?;
    // `--yes-including-revisions` implies `--yes` for anything else
    // in this command that reads `yes` (there is nothing else today,
    // but keeping the invariant explicit here means a future reader
    // of `yes` doesn't have to know about the other flag).
    let yes = yes || yes_including_revisions;

    // Canonicalize bash grants: trim each one, then validate. Pass trimmed
    // grants to the runner so build_argv interpolates canonicalized values.
    let agent_bash: Vec<String> = agent_bash
        .into_iter()
        .map(|g| g.trim().to_string())
        .collect();

    // Validate bash grants before anything runs. Nothing has been spawned
    // yet at any of these flag-rail checks, so a plain `process::exit` here
    // (rather than a returned code) can't skip cleanup of anything live.
    for grant in &agent_bash {
        if let Err(e) = topodb_sgh::runner::rails::validate_bash_grant(grant) {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    }

    // Flag rails: provider/base-url/agent-bash compatibility, and
    // the compiled-feature check for the requested provider. These
    // must run before the graph file is even read — a bad flag
    // combination is a rail violation, not something that depends
    // on the graph.
    validate_provider_flags(provider, &base_url, !agent_bash.is_empty());
    #[cfg(not(feature = "claude-code"))]
    if provider == Provider::ClaudeCode {
        eprintln!("error: this sgh was built without the claude-code feature");
        std::process::exit(2);
    }
    // No provider-specific replan rail here: an HTTP provider missing its
    // compiled feature is already caught below when the provider client is
    // built (`build_http_provider`), whether or not `--replan` is set.

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

    let mcp_argv = match agent_mcp.as_deref() {
        Some(cmd) => match topodb_sgh::runner::rails::validate_mcp_server_command(cmd) {
            Ok(argv) => Some(argv),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };

    // claude-code path: built eagerly, in the same place (before `Db::open`)
    // and order as before this task — the mcp-config file is temp-dir JSON,
    // not a spawned process, so nothing runs ahead of the approval gate
    // below.
    #[cfg(feature = "claude-code")]
    let mut runner: Option<BuiltRunner> = if provider == Provider::ClaudeCode {
        let mcp_wiring = match &mcp_argv {
            Some(argv) => Some(topodb_sgh::runner::claude::McpWiring {
                config_path: write_mcp_config(argv)?.to_string_lossy().into_owned(),
            }),
            None => None,
        };
        Some(BuiltRunner::Claude(
            ClaudeCodeRunner::new(model.clone(), agent_bash.clone(), mcp_wiring)
                .with_deadline(agent_timeout)
                .with_cancel(cancel.clone()),
        ))
    } else {
        None
    };
    #[cfg(not(feature = "claude-code"))]
    let mut runner: Option<BuiltRunner> = None;

    let db = Db::open(db_path)?;
    let command_runner = ShellCommandRunner::new(std::time::Duration::from_secs(command_timeout))
        .with_cancel(cancel.clone());

    // HTTP providers: the provider client is built now (from_env
    // only reads env vars — no IO), but the MCP bridge spawns a
    // subprocess, so it waits until after the first gate approval
    // in the loop below (`runner.is_none()` there).
    let mut pending_http_provider: Option<Box<dyn topodb_sgh::provider::ChatProvider>> =
        match provider {
            Provider::ClaudeCode => None,
            Provider::Anthropic | Provider::Openai => Some(build_http_provider(
                provider,
                model.clone(),
                base_url.clone(),
            )),
        };

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

        let grants = grants_preview(&agent_bash);
        if !grants.is_empty() {
            println!("\nAgent-node Bash grants (additive; agent prompts are ungated):");
            for grant in &grants {
                println!("  {grant}:*");
            }
        }

        print_unconstrained(&current);

        // From here on a bridge/runner from a previous loop iteration may
        // already be alive (a replan revision re-enters this loop with
        // `runner` already `Some`), so every exit below returns a code
        // instead of calling `std::process::exit` — the caller in `main`
        // only calls that once this function, and everything it owns, has
        // already dropped.
        if let Some(err) = mcp_pairing_error(&current, agent_mcp.as_deref()) {
            eprintln!("error: {err}");
            return Ok(2);
        }

        if let Some(cmd) = &agent_mcp {
            println!();
            let lines = match provider {
                Provider::ClaudeCode => {
                    #[cfg(feature = "claude-code")]
                    {
                        mcp_gate_lines(&current, cmd)
                    }
                    #[cfg(not(feature = "claude-code"))]
                    {
                        unreachable!(
                            "claude-code provider without the feature already \
                             exited at the flag-rail stage"
                        )
                    }
                }
                Provider::Anthropic | Provider::Openai => http_mcp_gate_lines(&current, cmd),
            };
            for line in lines {
                println!("{line}");
            }
        }

        // The gate preview above (commands + grants) prints on stdout
        // whether or not a prompt follows, so a --yes run is never
        // silent about the widened surface — no separate stderr echo.
        if needs_prompt(is_revision, yes, yes_including_revisions) {
            println!("\nProceed? [y/N]");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !line.trim().eq_ignore_ascii_case("y") {
                println!("aborted");
                return Ok(0);
            }
        }

        // HTTP providers only: build the runner now, post-approval —
        // this is where the MCP bridge (if the graph opts in) spawns
        // its subprocess, so no server starts for a run the operator
        // rejects. Happens once; subsequent loop iterations (replan
        // revisions) reuse the same runner and bridge.
        if runner.is_none() {
            #[cfg(feature = "http")]
            {
                let bridge = if !mcp_nodes(&current).is_empty() {
                    // mcp_pairing_error above already guarantees
                    // mcp_argv is Some whenever a node opts in.
                    let argv = mcp_argv
                        .as_ref()
                        .expect("mcp_pairing_error enforces this pairing");
                    match topodb_sgh::mcp_bridge::McpBridge::spawn(argv) {
                        Ok(b) => Some(b),
                        Err(e) => {
                            eprintln!("error: {e}");
                            return Ok(2);
                        }
                    }
                } else {
                    None
                };
                let provider_client = pending_http_provider
                    .take()
                    .expect("pending_http_provider is Some whenever provider != ClaudeCode");
                let mut http_runner = topodb_sgh::runner::http::HttpChatRunner::new(
                    provider_client,
                    model.clone(),
                    bridge,
                );
                http_runner.node_deadline = agent_timeout;
                http_runner.cancel = Some(cancel.clone());
                runner = Some(BuiltRunner::Http(Box::new(http_runner)));
            }
        }
        let runner_ref = runner
            .as_ref()
            .expect("built either eagerly (claude-code) or just above (http)")
            .as_agent_runner();

        let run_id = ulid::Ulid::new().to_string();
        let now = 1;
        let store = RunStore::create(&db, &run_id, &current, now)?;
        let mut ex = Executor::new(store, current.clone(), runner_ref)
            .with_command_runner(&command_runner)
            .with_cancel(cancel.clone())
            .with_max_inflight(max_inflight);
        let report = ex.run(now + 1)?;

        println!("\nrun {run_id}");
        println!("  succeeded: {:?}", report.succeeded);
        println!("  blocked:   {:?}", report.blocked);
        println!("  skipped:   {:?}", report.skipped);
        for id in &report.blocked {
            // A failed node prints its reason; a gate blocked on purpose
            // and has none — leave it to the exit-3 checkpoint message
            // below rather than labelling an intentional halt a failure.
            if let Some(reason) = report.blocked_reasons.get(id) {
                println!("    {id}: {}", one_line(reason));
            }
        }
        println!(
            "  model calls: {} (bound was {})",
            report.model_calls, bound.agent_calls
        );
        println!(
            "  command runs: {} (bound was {})",
            report.command_runs, bound.command_runs
        );

        if report.blocked.is_empty() {
            return Ok(0);
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
                return Ok(exit_code(&Outcome::HaltedAtCheckpoint));
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
                return Ok(1);
            }
            Step::Replan(n) => {
                replans_used = n;
            }
        }
        println!("{}", replan_banner(replans_used, max_replans));

        // The planner mirrors whatever `--provider` executed the run: a
        // claude-code run replans with `claude_planner`, an HTTP-provider
        // run replans with a fresh `ApiBackend` over the same provider.
        let planner =
            build_replan_planner(provider, model.clone(), base_url.clone(), agent_timeout);
        let revised = match propose_revision(&planner, &current, &ctx) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("error: replan failed: {e}");
                return Ok(1);
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
                return Ok(2);
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

/// The `plan` subcommand's body. Never builds a bridge/persistent child
/// process, so — unlike `run_cmd` — plain `std::process::exit` throughout is
/// fine; kept as a function only for symmetry with `run_cmd`'s call site in
/// `main`.
#[allow(clippy::too_many_arguments)]
fn plan_cmd(
    goal: String,
    out: Option<PathBuf>,
    context: Option<String>,
    model: Option<String>,
    provider: Provider,
    base_url: Option<String>,
    max_attempts: u32,
    agent_timeout: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent_timeout = Duration::from_secs(agent_timeout);
    // Flag rails before anything else, same as `run`.
    validate_provider_flags(provider, &base_url, false);
    #[cfg(not(feature = "claude-code"))]
    if provider == Provider::ClaudeCode {
        eprintln!("error: this sgh was built without the claude-code feature");
        std::process::exit(2);
    }

    {
        let planner: topodb_sgh::planner::BoundedPlanner = match provider {
            Provider::ClaudeCode => {
                #[cfg(feature = "claude-code")]
                {
                    claude_planner_with_timeout(model, max_attempts, agent_timeout)
                }
                #[cfg(not(feature = "claude-code"))]
                {
                    unreachable!("claude-code provider without the feature already rejected above")
                }
            }
            Provider::Anthropic | Provider::Openai => {
                #[cfg(feature = "http")]
                {
                    let provider_client = build_http_provider(provider, model.clone(), base_url);
                    let backend = topodb_sgh::planner::api::ApiBackend::new(provider_client, model)
                        .with_timeout(agent_timeout);
                    topodb_sgh::planner::BoundedPlanner::with_backend(
                        Box::new(backend),
                        max_attempts,
                    )
                }
                #[cfg(not(feature = "http"))]
                {
                    eprintln!("error: this sgh was built without the http feature");
                    std::process::exit(2);
                }
            }
        };
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

    fn graph_with_tools(tools_on_a: &[&str]) -> Validated {
        let yaml = if tools_on_a.is_empty() {
            "version: 1\ngoal: g\nnodes:\n  - id: a\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n  - id: b\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n".to_string()
        } else {
            format!(
                "version: 1\ngoal: g\nnodes:\n  - id: a\n    kind: agent\n    prompt: p\n    tools: [{}]\n    budget: {{retries: 0, repairs: 0}}\n  - id: b\n    kind: agent\n    prompt: p\n    budget: {{retries: 0, repairs: 0}}\n",
                tools_on_a.join(", ")
            )
        };
        validate(&Graph::from_yaml(&yaml).expect("parses")).expect("valid")
    }

    #[test]
    fn grants_preview_returns_grants_unchanged() {
        let grants = vec!["topodb".to_string()];
        let lines = grants_preview(&grants);
        assert_eq!(lines, vec!["topodb".to_string()]);
    }

    #[test]
    fn grants_preview_empty_grants_returns_empty() {
        let grants: Vec<String> = vec![];
        let lines = grants_preview(&grants);
        assert!(lines.is_empty());
    }

    #[test]
    fn grants_preview_multiple_grants() {
        let grants = vec!["topodb".to_string(), "cargo".to_string()];
        let lines = grants_preview(&grants);
        assert_eq!(lines, vec!["topodb".to_string(), "cargo".to_string()]);
    }

    #[test]
    fn mcp_nodes_lists_opted_in_agent_ids_in_order() {
        let v = graph_with_tools(&["topodb"]);
        assert_eq!(mcp_nodes(&v), vec!["a".to_string()]);
        let v_none = graph_with_tools(&[]);
        assert!(mcp_nodes(&v_none).is_empty());
    }

    #[test]
    fn pairing_error_when_tools_without_flag() {
        let v = graph_with_tools(&["topodb"]);
        let err = mcp_pairing_error(&v, None).expect("must error");
        assert!(
            err.contains("node(s) a ") && err.contains("--agent-mcp"),
            "{err}"
        );
        assert!(mcp_pairing_error(&v, Some("cmd")).is_none());
        let v_none = graph_with_tools(&[]);
        assert!(mcp_pairing_error(&v_none, None).is_none());
    }

    #[test]
    #[cfg(feature = "claude-code")]
    fn mcp_config_file_names_the_topodb_server() {
        let path = write_mcp_config(&[
            "/abs/topodb-mcp".to_string(),
            "--db".to_string(),
            "/tmp/m.redb".to_string(),
        ])
        .unwrap();
        let body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["mcpServers"]["topodb"]["command"], "/abs/topodb-mcp");
        assert_eq!(body["mcpServers"]["topodb"]["args"][0], "--db");
        assert_eq!(body["mcpServers"]["topodb"]["args"][1], "/tmp/m.redb");
        let _ = std::fs::remove_file(path);
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

    // An agent node with no declared `output.schema` is unconstrained: the
    // executor validates output only when a schema is present, so whatever the
    // node returns is accepted. Its success is self-reported. The gate should
    // say so, because "succeeded" on such a node is not evidence of work.

    #[test]
    fn unconstrained_agents_lists_agent_nodes_with_no_declared_output() {
        let v = validated(
            "version: 1\ngoal: g\nnodes:\n\
             - {id: survey, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
             - {id: build, kind: command, run: 'cargo build', budget: {retries: 0, repairs: 0}}\n\
             - {id: checked, kind: agent, prompt: p, output: {schema: {type: object}}, budget: {retries: 0, repairs: 0}}\n\
             - {id: verify, kind: command, run: 'cargo test', needs: [checked], budget: {retries: 0, repairs: 0}}\n",
        );
        assert_eq!(
            unconstrained_agents(&v),
            vec!["survey".to_string()],
            "only agent nodes lacking an output schema are unconstrained"
        );
    }

    #[test]
    fn unconstrained_agents_is_empty_when_every_agent_declares_output() {
        let v = validated(
            "version: 1\ngoal: g\nnodes:\n\
             - {id: a, kind: agent, prompt: p, output: {schema: {type: object}}, budget: {retries: 0, repairs: 0}}\n\
             - {id: b, kind: command, run: 'true', needs: [a], budget: {retries: 0, repairs: 0}}\n",
        );
        assert!(unconstrained_agents(&v).is_empty());
    }

    #[test]
    fn one_line_collapses_newlines_so_a_reason_stays_one_row() {
        assert_eq!(
            one_line("denied WebFetch\n  and stopped"),
            "denied WebFetch and stopped"
        );
    }

    #[test]
    fn one_line_truncates_a_long_reason() {
        let long = "x".repeat(400);
        let out = one_line(&long);
        let chars = out.chars().count();
        assert!(chars <= 201, "kept short enough to read: {chars}");
        assert!(out.ends_with('…'));
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
            blocked_reasons: std::collections::BTreeMap::new(),
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

    #[test]
    fn agent_mcp_flag_rejects_repeats_on_run_subcommand() {
        use clap::Parser;
        let result = Cli::try_parse_from(vec![
            "sgh",
            "run",
            "graph.yaml",
            "--agent-mcp",
            "cmd1",
            "--agent-mcp",
            "cmd2",
        ]);
        assert!(result.is_err(), "repeated --agent-mcp must be rejected");
    }

    #[test]
    fn agent_mcp_flag_rejects_repeats_on_validate_subcommand() {
        use clap::Parser;
        let result = Cli::try_parse_from(vec![
            "sgh",
            "validate",
            "graph.yaml",
            "--agent-mcp",
            "cmd1",
            "--agent-mcp",
            "cmd2",
        ]);
        assert!(result.is_err(), "repeated --agent-mcp must be rejected");
    }

    #[test]
    #[cfg(feature = "claude-code")]
    fn mcp_gate_lines_formats_output_correctly_with_opted_in_nodes() {
        let v = graph_with_tools(&["topodb"]);
        let lines = mcp_gate_lines(&v, "/abs/topodb-mcp --db /tmp/m.redb");
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "Agent-node MCP server (additive; agent prompts are ungated):"
        );
        assert_eq!(lines[1], "  /abs/topodb-mcp --db /tmp/m.redb");
        assert_eq!(lines[2], "  tools: mcp__topodb -> nodes: a");
    }

    #[test]
    #[cfg(feature = "claude-code")]
    fn mcp_gate_lines_formats_output_correctly_without_opted_in_nodes() {
        let v = graph_with_tools(&[]);
        let lines = mcp_gate_lines(&v, "/abs/topodb-mcp --db /tmp/m.redb");
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "Agent-node MCP server (additive; agent prompts are ungated):"
        );
        assert_eq!(lines[1], "  /abs/topodb-mcp --db /tmp/m.redb");
        assert_eq!(
            lines[2],
            "  tools: mcp__topodb -> no node opts in (flag is inert for this graph)"
        );
    }
}
