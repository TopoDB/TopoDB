use std::process::Command;

use super::{build_plan_prompt, PlanRequest, Planner, PlannerError};
use crate::schema::validate::{validate, ValidationError};
use crate::schema::Graph;

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

/// Shells out to `claude -p`, mirroring `runner::claude::ClaudeCodeRunner`.
pub struct ClaudeBackend {
    model: Option<String>,
}

impl PlanBackend for ClaudeBackend {
    fn complete(&self, prompt: &str) -> Result<String, PlannerError> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p").arg(prompt);
        if let Some(m) = &self.model {
            cmd.arg("--model").arg(m);
        }
        let out = cmd
            .output()
            .map_err(|e| PlannerError::Runner(format!("spawning claude: {e}")))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            return Err(PlannerError::Runner(format!(
                "claude exited with {}: {}",
                out.status,
                stderr.trim()
            )));
        }
        String::from_utf8(out.stdout)
            .map_err(|_| PlannerError::Runner("claude produced invalid utf-8".into()))
    }
}

/// Compiles a goal into a graph, retrying a **bounded** number of times when
/// the produced document fails to parse or validate.
///
/// The bound is not incidental. An unbounded "keep asking until it validates"
/// loop is precisely the unbounded recovery this project argues against, so
/// `max_attempts` is fixed up front and exhaustion is a reported error rather
/// than another attempt.
pub struct ClaudePlanner {
    backend: Box<dyn PlanBackend>,
    max_attempts: u32,
}

impl ClaudePlanner {
    pub fn new(model: Option<String>, max_attempts: u32) -> Self {
        ClaudePlanner {
            backend: Box::new(ClaudeBackend { model }),
            max_attempts: max_attempts.max(1),
        }
    }

    pub fn with_backend(backend: Box<dyn PlanBackend>, max_attempts: u32) -> Self {
        ClaudePlanner {
            backend,
            max_attempts: max_attempts.max(1),
        }
    }
}

impl Planner for ClaudePlanner {
    fn plan(&self, req: &PlanRequest) -> Result<Graph, PlannerError> {
        let mut errors: Vec<ValidationError> = Vec::new();
        let mut last_yaml: Option<String> = None;

        for _ in 0..self.max_attempts {
            let prompt = build_plan_prompt(req, &errors, last_yaml.as_deref());
            let raw = self.backend.complete(&prompt)?;
            let yaml = strip_fences(&raw);

            match Graph::from_yaml(&yaml) {
                Ok(graph) => match validate(&graph) {
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
