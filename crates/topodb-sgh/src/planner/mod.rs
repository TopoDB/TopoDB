#[cfg(feature = "http")]
pub mod api;
#[cfg(feature = "claude-code")]
pub mod claude;
pub mod mock;

use crate::schema::validate::ValidationError;
use crate::schema::Graph;

#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    #[error("planner produced unparseable yaml: {0}")]
    Yaml(String),
    #[error(
        "planner failed to produce a valid graph in {attempts} attempt(s); last errors: {errors:?}"
    )]
    Exhausted {
        attempts: u32,
        errors: Vec<ValidationError>,
    },
    #[error("planner backend error: {0}")]
    Runner(String),
}

/// What the planner is asked to turn into a graph.
#[derive(Debug, Clone)]
pub struct PlanRequest {
    pub goal: String,
    /// Optional grounding — repository facts, constraints, prior findings.
    pub context: Option<String>,
}

/// The text-completion backend the planner drives. Injectable so the
/// bounded retry loop is testable without spawning a model.
pub trait PlanBackend: Send + Sync {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError>;
}

impl<T: PlanBackend + ?Sized> PlanBackend for std::sync::Arc<T> {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError> {
        (**self).complete(prompt)
    }
}

pub trait Planner: Send + Sync {
    /// Compile a goal into a graph. Implementations MUST return only graphs
    /// that pass `schema::validate::validate`, and MUST bound their own
    /// retry attempts — an unbounded planning loop would reintroduce exactly
    /// the unbounded recovery this project exists to remove.
    fn plan(&self, req: &PlanRequest) -> Result<Graph, PlannerError>;
}

/// Assemble the planning prompt. Kept free of any model call so it is
/// unit-testable without a `claude` binary present.
///
/// `previous` and `previous_yaml` are empty/None on the first attempt and
/// carry the prior rejection on a retry, so the model sees exactly what was
/// wrong rather than being asked again blind.
pub fn build_plan_prompt(
    req: &PlanRequest,
    previous: &[ValidationError],
    previous_yaml: Option<&str>,
) -> String {
    let mut p = String::new();

    p.push_str("Compile this goal into an execution graph.\n\n## Goal\n\n");
    p.push_str(&req.goal);
    p.push_str("\n\n");

    if let Some(ctx) = &req.context {
        p.push_str("## Context\n\n");
        p.push_str(ctx);
        p.push_str("\n\n");
    }

    p.push_str(
        "## Output format\n\n\
         Reply with bare YAML and nothing else — no prose, no code fences.\n\n\
         version: 1\n\
         goal: \"<restate the goal>\"\n\
         nodes:\n\
         \x20 - id: <unique-id>\n\
         \x20   kind: agent | command | gate\n\
         \x20   needs: [<ids of steps this depends on>]\n\
         \x20   prompt: \"<instructions>\"      # required for kind: agent\n\
         \x20   run: \"<shell command>\"        # required for kind: command\n\
         \x20   output:\n\
         \x20     schema: {<JSON Schema>}     # optional; if set, output MUST match\n\
         \x20   tools: [topodb]              # optional, agent nodes ONLY; use only if the step must read/write persistent memory\n\
         \x20   budget: {retries: <n>, repairs: <n>}   # required on every node\n\n",
    );

    p.push_str(
        "## Rules\n\n\
         - Every node id is unique, and every entry in `needs` names a node that exists.\n\
         - The graph must be acyclic.\n\
         - `kind: agent` requires `prompt`. `kind: command` requires `run`.\n\
         - `kind: gate` halts the run before its dependents and requires neither `prompt` \
         nor `run`. Today a gate always blocks — there is no interactive resume yet — so \
         use it to stop the run before a destructive or irreversible step, not as a routine \
         checkpoint.\n\
         - `output.schema`, when present, must be a valid JSON Schema document that \
         compiles — a schema that fails to compile rejects the whole graph, costing a retry. \
         Keep it to simple constructs (`type`, `properties`, `required`, `items`).\n\
         - An agent node that produces or changes anything must declare `output.schema` \
         stating what it did — files written, tests added, defects fixed. A node with no \
         declared output is accepted whatever it returns, so its success means only that the \
         model replied, not that the work happened.\n\
         - An agent's own report is not evidence, and this is enforced: an agent node that \
         declares `output.schema` with no `kind: command` node anywhere downstream of it is \
         REJECTED, costing a retry. Every claim needs a command that depends on it and checks \
         it. Pair \"wrote the tests\" with a command that runs them and fails when there are \
         none; pair \"fixed the defect\" with the command that reproduces it. A check that \
         passes when nothing happened is not a check — `cargo test` on a crate with zero tests \
         exits 0, so assert a positive fact (a nonzero count) rather than a clean exit.\n\
         - `budget` is required on every node. Use `retries: 0` where a retry cannot help \
         (a deterministic failure), and small values elsewhere — the budget is the run's \
         worst-case cost and a human approves it before anything executes.\n\
         - A node receives ONLY the outputs of the nodes in its `needs`. Nothing else from \
         the run is visible to it, so declare every dependency a step actually needs.\n\
         - Prefer `kind: command` for deterministic work (builds, tests, file moves): it \
         costs no model calls and is exactly reproducible.\n\
         - Commands run in a shell and are shown to a human for approval before executing.\n\
         - `tools: [topodb]` opts an agent node into TopoDB memory tools (MCP). Emit it \
         ONLY when the goal or context says persistent memory is available and the step \
         genuinely needs to remember or recall across runs — a graph that declares it \
         requires the operator to pass `--agent-mcp` at run time.\n\n",
    );

    if !previous.is_empty() {
        p.push_str("## Your previous attempt was rejected\n\n");
        if let Some(yaml) = previous_yaml {
            p.push_str("You produced:\n\n```yaml\n");
            p.push_str(yaml);
            p.push_str("\n```\n\n");
        }
        p.push_str("The validator reported:\n\n");
        for e in previous {
            p.push_str(&format!("- {e}\n"));
        }
        p.push_str("\nFix these specific problems and reply with the corrected YAML.\n");
    }

    p
}

/// Compiles a goal into a graph, retrying a **bounded** number of times when
/// the produced document fails to parse or validate.
///
/// The bound is not incidental. An unbounded "keep asking until it validates"
/// loop is precisely the unbounded recovery this project argues against, so
/// `max_attempts` is fixed up front and exhaustion is a reported error rather
/// than another attempt.
pub struct BoundedPlanner {
    backend: Box<dyn PlanBackend>,
    max_attempts: u32,
}

impl BoundedPlanner {
    pub fn with_backend(backend: Box<dyn PlanBackend>, max_attempts: u32) -> Self {
        BoundedPlanner {
            backend,
            max_attempts: max_attempts.max(1),
        }
    }
}

impl Planner for BoundedPlanner {
    fn plan(&self, req: &PlanRequest) -> Result<Graph, PlannerError> {
        let mut errors: Vec<ValidationError> = Vec::new();
        let mut last_yaml: Option<String> = None;

        for _ in 0..self.max_attempts {
            let prompt = build_plan_prompt(req, &errors, last_yaml.as_deref());
            let raw = self.backend.complete(&prompt)?;
            let yaml = strip_fences(&raw);

            match Graph::from_yaml(&yaml) {
                Ok(graph) => match crate::schema::validate::validate(&graph) {
                    Ok(_) => return Ok(graph),
                    Err(errs) => {
                        errors = errs;
                        last_yaml = Some(yaml);
                    }
                },
                Err(e) => {
                    // A parse failure is fed back the same way a validation
                    // failure is — the model gets the specific complaint.
                    errors = vec![ValidationError::InvalidSchema {
                        node: "<document>".into(),
                        reason: e.to_string(),
                    }];
                    last_yaml = Some(yaml);
                }
            }
        }

        Err(PlannerError::Exhausted {
            attempts: self.max_attempts,
            errors,
        })
    }
}

/// Models often wrap YAML in code fences despite instructions. Strip one
/// leading/trailing fence rather than failing the attempt over formatting.
/// Both the `yaml` and the short `yml` language tags are recognized — a
/// model using the short tag would otherwise cost a full retry attempt.
fn strip_fences(raw: &str) -> String {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest
        .strip_prefix("yaml")
        .or_else(|| rest.strip_prefix("yml"))
        .unwrap_or(rest);
    rest.trim_start_matches('\n')
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim()
        .to_string()
}

#[cfg(test)]
mod strip_fences_tests {
    use super::strip_fences;

    #[test]
    fn strips_yaml_fence() {
        assert_eq!(strip_fences("```yaml\nversion: 1\n```"), "version: 1");
    }

    #[test]
    fn strips_short_yml_fence() {
        assert_eq!(strip_fences("```yml\nversion: 1\n```"), "version: 1");
    }

    #[test]
    fn strips_bare_fence_with_no_language_tag() {
        assert_eq!(strip_fences("```\nversion: 1\n```"), "version: 1");
    }

    #[test]
    fn leaves_unfenced_yaml_untouched() {
        assert_eq!(strip_fences("version: 1"), "version: 1");
    }
}
