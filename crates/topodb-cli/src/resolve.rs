//! Precedence resolution (flag → env → .topodb.toml → default) for the
//! db path, scope, and output format, each tagged with the Source it came
//! from. Pure logic — no process globals read here; callers pass env/cwd in.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Flag,
    Env,
    Config(PathBuf),
    Default,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Flag => "flag".to_string(),
            Source::Env => "env".to_string(),
            Source::Config(p) => format!("config {}", p.display()),
            Source::Default => "default".to_string(),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ProjectConfig {
    pub db: Option<String>,
    pub scope: Option<String>,
    pub format: Option<String>,
    pub path: Option<PathBuf>,
    pub unknown_keys: Vec<String>,
}

pub fn load_project_config(start_dir: &Path) -> Result<Option<ProjectConfig>, String> {
    let mut dir = start_dir;
    loop {
        let candidate = dir.join(".topodb.toml");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate)
                .map_err(|e| format!("reading {}: {e}", candidate.display()))?;
            let table: toml::Table = text
                .parse()
                .map_err(|e| format!("parsing {}: {e}", candidate.display()))?;
            let mut cfg = ProjectConfig {
                path: Some(candidate.clone()),
                ..ProjectConfig::default()
            };
            let as_string = |key: &str, v: &toml::Value| -> Result<String, String> {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    format!(
                        "{}: key `{key}` must be a string, got {}",
                        candidate.display(),
                        v.type_str()
                    )
                })
            };
            for (k, v) in &table {
                match k.as_str() {
                    "db" => cfg.db = Some(as_string("db", v)?),
                    "scope" => cfg.scope = Some(as_string("scope", v)?),
                    "format" => cfg.format = Some(as_string("format", v)?),
                    other => cfg.unknown_keys.push(other.to_string()),
                }
            }
            return Ok(Some(cfg));
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) {
        std::fs::write(dir.join(".topodb.toml"), body).unwrap();
    }

    #[test]
    fn none_when_no_config_on_path() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load_project_config(d.path()).unwrap(), None);
    }

    #[test]
    fn parses_known_keys() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "db = \"~/x.redb\"\nscope = \"shared\"\nformat = \"text\"\n");
        let cfg = load_project_config(d.path()).unwrap().unwrap();
        assert_eq!(cfg.db.as_deref(), Some("~/x.redb"));
        assert_eq!(cfg.scope.as_deref(), Some("shared"));
        assert_eq!(cfg.format.as_deref(), Some("text"));
        assert_eq!(cfg.path, Some(d.path().join(".topodb.toml")));
    }

    #[test]
    fn found_in_parent_dir() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "scope = \"shared\"\n");
        let child = d.path().join("a/b");
        std::fs::create_dir_all(&child).unwrap();
        let cfg = load_project_config(&child).unwrap().unwrap();
        assert_eq!(cfg.scope.as_deref(), Some("shared"));
    }

    #[test]
    fn unknown_key_is_collected_not_error() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "scope = \"shared\"\nnope = 1\n");
        let cfg = load_project_config(d.path()).unwrap().unwrap();
        assert_eq!(cfg.unknown_keys, vec!["nope".to_string()]);
    }

    #[test]
    fn malformed_toml_is_error_naming_file() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "db = = =");
        let err = load_project_config(d.path()).unwrap_err();
        assert!(err.contains(".topodb.toml"), "err was: {err}");
    }

    #[test]
    fn wrong_typed_key_is_error() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "db = 42\n");
        let err = load_project_config(d.path()).unwrap_err();
        assert!(err.contains("db"), "err was: {err}");
    }
}
