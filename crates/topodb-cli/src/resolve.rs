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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Json,
    Text,
}

#[derive(Debug)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

/// Expand a leading `~/` (or a bare `~`) using `home`. Anything else is
/// returned verbatim. `home == None` leaves `~` untouched.
pub fn expand_home(path: &str, home: Option<&str>) -> PathBuf {
    match home {
        Some(h) if path == "~" => PathBuf::from(h),
        Some(h) if path.starts_with("~/") => PathBuf::from(h).join(&path[2..]),
        _ => PathBuf::from(path),
    }
}

pub fn resolve_db(
    flag: Option<PathBuf>,
    env: Option<String>,
    config: Option<(&str, &Path)>,
    home: Option<&str>,
) -> Resolved<PathBuf> {
    if let Some(f) = flag {
        return Resolved { value: f, source: Source::Flag };
    }
    if let Some(e) = env {
        return Resolved { value: expand_home(&e, home), source: Source::Env };
    }
    if let Some((c, p)) = config {
        return Resolved { value: expand_home(c, home), source: Source::Config(p.to_path_buf()) };
    }
    Resolved {
        value: expand_home("~/.topodb/memory.redb", home),
        source: Source::Default,
    }
}

pub fn resolve_scope_str(
    flag: Option<String>,
    env: Option<String>,
    config: Option<(&str, &Path)>,
) -> Resolved<String> {
    if let Some(f) = flag {
        return Resolved { value: f, source: Source::Flag };
    }
    if let Some(e) = env {
        return Resolved { value: e, source: Source::Env };
    }
    if let Some((c, p)) = config {
        return Resolved { value: c.to_string(), source: Source::Config(p.to_path_buf()) };
    }
    Resolved { value: "shared".to_string(), source: Source::Default }
}

fn parse_format(s: &str) -> Result<Format, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "json" => Ok(Format::Json),
        "text" => Ok(Format::Text),
        other => Err(format!("invalid format {other:?} (expected \"json\" or \"text\")")),
    }
}

pub fn resolve_format(
    flag: Option<Format>,
    env: Option<String>,
    config: Option<(&str, &Path)>,
    is_terminal: bool,
) -> Result<Resolved<Format>, String> {
    if let Some(f) = flag {
        return Ok(Resolved { value: f, source: Source::Flag });
    }
    if let Some(e) = env {
        return Ok(Resolved { value: parse_format(&e)?, source: Source::Env });
    }
    if let Some((c, p)) = config {
        return Ok(Resolved { value: parse_format(c)?, source: Source::Config(p.to_path_buf()) });
    }
    Ok(Resolved {
        value: if is_terminal { Format::Text } else { Format::Json },
        source: Source::Default,
    })
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

    #[test]
    fn expand_home_replaces_leading_tilde() {
        assert_eq!(
            expand_home("~/.topodb/memory.redb", Some("/home/x")),
            PathBuf::from("/home/x/.topodb/memory.redb")
        );
        assert_eq!(expand_home("/abs/p", Some("/home/x")), PathBuf::from("/abs/p"));
        assert_eq!(expand_home("~/p", None), PathBuf::from("~/p"));
    }

    #[test]
    fn db_precedence_flag_over_env_over_config_over_default() {
        let p = PathBuf::from("/cfg/.topodb.toml");
        // flag wins
        let r = resolve_db(Some("/flag.redb".into()), Some("/env.redb".into()),
            Some(("/cfg.redb", &p)), Some("/home"));
        assert_eq!(r.value, PathBuf::from("/flag.redb"));
        assert_eq!(r.source, Source::Flag);
        // env next
        let r = resolve_db(None, Some("/env.redb".into()), Some(("/cfg.redb", &p)), Some("/home"));
        assert_eq!(r.value, PathBuf::from("/env.redb"));
        assert_eq!(r.source, Source::Env);
        // config next (with ~ expansion)
        let r = resolve_db(None, None, Some(("~/c.redb", &p)), Some("/home"));
        assert_eq!(r.value, PathBuf::from("/home/c.redb"));
        assert_eq!(r.source, Source::Config(p.clone()));
        // default last
        let r = resolve_db(None, None, None, Some("/home"));
        assert_eq!(r.value, PathBuf::from("/home/.topodb/memory.redb"));
        assert_eq!(r.source, Source::Default);
    }

    #[test]
    fn scope_precedence_and_default_shared() {
        let p = PathBuf::from("/cfg/.topodb.toml");
        assert_eq!(resolve_scope_str(Some("A".into()), Some("B".into()), Some(("C", &p))).value, "A");
        assert_eq!(resolve_scope_str(None, Some("B".into()), Some(("C", &p))).value, "B");
        assert_eq!(resolve_scope_str(None, None, Some(("C", &p))).value, "C");
        let d = resolve_scope_str(None, None, None);
        assert_eq!(d.value, "shared");
        assert_eq!(d.source, Source::Default);
    }

    #[test]
    fn format_default_is_tty_aware_and_override_wins() {
        assert_eq!(resolve_format(None, None, None, true).unwrap().value, Format::Text);
        assert_eq!(resolve_format(None, None, None, false).unwrap().value, Format::Json);
        // explicit env overrides a TTY default
        assert_eq!(resolve_format(None, Some("json".into()), None, true).unwrap().value, Format::Json);
        // flag wins over env
        assert_eq!(resolve_format(Some(Format::Text), Some("json".into()), None, false).unwrap().value, Format::Text);
    }

    #[test]
    fn format_bad_value_errors() {
        let err = resolve_format(None, Some("yaml".into()), None, false).unwrap_err();
        assert!(err.contains("yaml"), "err was: {err}");
    }
}
