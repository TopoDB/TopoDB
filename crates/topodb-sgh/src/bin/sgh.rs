use std::path::PathBuf;

use clap::{Parser, Subcommand};
use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::claude::ClaudeCodeRunner;
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
    },
}

/// Command nodes parse, validate, and cost exactly like any other node, but
/// there is no shell execution path anywhere in this crate yet: the executor
/// dispatches `NodeKind::Command` through `AgentRunner` exactly like
/// `NodeKind::Agent`, which would send a shell command to a *model* as a
/// prompt rather than executing it. Real command execution is deferred to
/// v0.0.2 (a `CommandRunner` shell path). Rather than ship that silently
/// wrong behavior, the CLI refuses any graph containing a command node.
/// Returns the offending node ids in declaration order.
fn unsupported_command_nodes(v: &Validated) -> Vec<String> {
    v.graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Command)
        .map(|n| n.id.clone())
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Validate { graph } => {
            let src = std::fs::read_to_string(&graph)?;
            let g = Graph::from_yaml(&src)?;
            match validate(&g) {
                Ok(v) => {
                    let offenders = unsupported_command_nodes(&v);
                    if !offenders.is_empty() {
                        eprintln!(
                            "error: command nodes are not supported until v0.0.2: {}",
                            offenders.join(", ")
                        );
                        std::process::exit(2);
                    }
                    println!("valid: {} node(s)", v.graph.nodes.len());
                    println!("{}", worst_case(&v));
                }
                Err(errors) => {
                    for e in &errors {
                        eprintln!("error: {e}");
                    }
                    std::process::exit(2);
                }
            }
        }
        Cmd::Run { graph, model, yes } => {
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

            let offenders = unsupported_command_nodes(&v);
            if !offenders.is_empty() {
                eprintln!(
                    "error: command nodes are not supported until v0.0.2: {}",
                    offenders.join(", ")
                );
                std::process::exit(2);
            }

            let bound = worst_case(&v);
            println!("Goal: {}", v.graph.goal);
            println!("Nodes: {}", v.graph.nodes.len());
            println!("Bound: {bound}");

            if !yes {
                println!("\nProceed? [y/N]");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if !line.trim().eq_ignore_ascii_case("y") {
                    println!("aborted");
                    return Ok(());
                }
            }

            let db = Db::open(&cli.db)?;
            let run_id = ulid::Ulid::new().to_string();
            let now = 1;
            let store = RunStore::create(&db, &run_id, &v, now)?;
            let runner = ClaudeCodeRunner::new(model);

            let mut ex = Executor::new(store, v, &runner);
            let report = ex.run(now + 1)?;

            println!("\nrun {run_id}");
            println!("  succeeded: {:?}", report.succeeded);
            println!("  blocked:   {:?}", report.blocked);
            println!("  skipped:   {:?}", report.skipped);
            println!("  model calls: {} (bound was {})", report.model_calls, bound.agent_calls);

            if !report.blocked.is_empty() {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::unsupported_command_nodes;
    use topodb_sgh::schema::validate::validate;
    use topodb_sgh::schema::Graph;

    #[test]
    fn flags_command_nodes_by_id() {
        let yaml = r#"
version: 1
goal: "test"
nodes:
  - id: survey
    kind: agent
    prompt: "Locate every call site"
    budget: {retries: 0, repairs: 0}
  - id: build
    kind: command
    run: "cargo build"
    needs: [survey]
    budget: {retries: 0, repairs: 0}
"#;
        let g = Graph::from_yaml(yaml).unwrap();
        let v = validate(&g).unwrap();
        assert_eq!(unsupported_command_nodes(&v), vec!["build".to_string()]);
    }

    #[test]
    fn empty_when_only_agent_and_gate_nodes() {
        let yaml = r#"
version: 1
goal: "test"
nodes:
  - id: survey
    kind: agent
    prompt: "Locate every call site"
    budget: {retries: 0, repairs: 0}
  - id: approve
    kind: gate
    needs: [survey]
    budget: {retries: 0, repairs: 0}
"#;
        let g = Graph::from_yaml(yaml).unwrap();
        let v = validate(&g).unwrap();
        assert!(unsupported_command_nodes(&v).is_empty());
    }
}
