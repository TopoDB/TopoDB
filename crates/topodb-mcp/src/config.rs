//! CLI parsing and the server configuration contract.
//!
//! CLI: `topodb-mcp --db <path> [--scope <ulid|shared>] [--spec <spec.json>]`
//! - `--scope`: the default scope for every tool call that omits an explicit
//!   `scope`. `"shared"` (case-insensitive) or omitted => [`Scope::Shared`];
//!   any other value is parsed as a ULID => [`Scope::Id`].
//! - `--spec`: path to a JSON file deserializing to [`IndexSpec`], honored
//!   verbatim (may reindex an existing db). Omitted => inherit the db's
//!   persisted spec on an existing file, or create a fresh db with the
//!   [built-in default spec](default_spec). See how `main` opens the db.
//!
//! Arg parsing is hand-rolled: the surface is three flags, so `clap` would add
//! a dependency and a proc-macro build for no real gain here.

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr;

use topodb::{IndexSpec, Scope, ScopeId};

/// Label/prop name constants, single-sourced in `topodb-json` (shared with
/// `topodb-cli`'s `create-entity`/`create-memory`) and re-exported here so
/// existing `topodb-mcp` call sites (`use crate::config::{ENTITY_LABEL, ...}`)
/// keep working unchanged.
pub use topodb_json::{ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP, MEMORY_LABEL};

/// Resolved server configuration (see the module docs for the CLI contract).
#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub default_scope: Scope,
    /// The spec parsed from an explicit `--spec` file, or `None` when the flag
    /// was omitted. `None` means "inherit the db's persisted spec" (see how
    /// `main` opens the db), NOT "use `default_spec()`" — the two diverge for
    /// an existing db: silently substituting the default would reindex it and
    /// drop its declared equality indexes.
    pub spec: Option<IndexSpec>,
}

/// The built-in default index spec used when `--spec` is omitted: equality on
/// `(Entity, name)`, text on `(Memory, content)`. Single-sourced in
/// `topodb-json` (shared with `topodb-cli`'s fresh-db bootstrap) so a
/// CLI-created db and an MCP-created db carry a byte-identical persisted
/// `index_spec` — either front end can serve the other's db via `open_stored`
/// with no reindex and no mis-declared index. Re-exported here so existing
/// `topodb-mcp` call sites (`config::default_spec()`) keep working unchanged.
pub use topodb_json::default_spec;

/// Human/JSON-facing rendering of a [`Scope`]: `"shared"` or the ULID string.
/// Single-sourced in `topodb-json`; re-exported here so existing
/// `topodb-mcp` call sites keep working unchanged.
pub use topodb_json::scope_label;

/// Parses a `--scope` value: `"shared"` (any case) => [`Scope::Shared`],
/// otherwise a ULID string => [`Scope::Id`].
fn parse_scope(s: &str) -> Result<Scope, Box<dyn Error>> {
    if s.eq_ignore_ascii_case("shared") {
        Ok(Scope::Shared)
    } else {
        let id = ScopeId::from_str(s).map_err(|e| {
            format!("invalid --scope value {s:?} (expected \"shared\" or a ULID): {e}")
        })?;
        Ok(Scope::Id(id))
    }
}

impl Config {
    /// Parses config from an argument iterator (excluding argv[0]). Returns a
    /// clear error for missing values, unknown flags, or a missing/invalid
    /// `--spec` file. Does NOT touch the filesystem for `--db` — the parent-dir
    /// check lives in `main` so this stays a pure parse.
    pub fn from_args<I>(args: I) -> Result<Self, Box<dyn Error>>
    where
        I: IntoIterator<Item = String>,
    {
        let mut db_path: Option<PathBuf> = None;
        let mut scope: Option<String> = None;
        let mut spec_path: Option<PathBuf> = None;

        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--db" => {
                    db_path = Some(it.next().ok_or("--db requires a <path> value")?.into());
                }
                "--scope" => {
                    scope = Some(it.next().ok_or("--scope requires a <ulid|shared> value")?);
                }
                "--spec" => {
                    spec_path = Some(
                        it.next()
                            .ok_or("--spec requires a <spec.json> value")?
                            .into(),
                    );
                }
                other => {
                    return Err(format!(
                        "unknown argument {other:?}; usage: topodb-mcp --db <path> [--scope <ulid|shared>] [--spec <spec.json>]"
                    )
                    .into());
                }
            }
        }

        let db_path = db_path.ok_or("missing required --db <path>")?;
        let default_scope = match scope {
            Some(s) => parse_scope(&s)?,
            None => Scope::Shared,
        };
        let spec = match spec_path {
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| format!("reading --spec {}: {e}", p.display()))?;
                Some(serde_json::from_str(&text).map_err(|e| {
                    format!("parsing --spec {} as IndexSpec JSON: {e}", p.display())
                })?)
            }
            None => None,
        };

        Ok(Config {
            db_path,
            default_scope,
            spec,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_scope_shared_and_no_spec() {
        let cfg = Config::from_args(argv(&["--db", "t.redb"])).unwrap();
        assert_eq!(cfg.db_path, PathBuf::from("t.redb"));
        assert!(matches!(cfg.default_scope, Scope::Shared));
        // No `--spec` => None ("inherit the db's persisted spec"), NOT
        // default_spec(): `main` only falls back to the default for a fresh db.
        assert!(cfg.spec.is_none());
    }

    #[test]
    fn explicit_spec_flag_is_parsed_to_some() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("spec.json");
        std::fs::write(
            &p,
            r#"{"equality":[{"label":"Person","prop":"handle"}],"text":[]}"#,
        )
        .unwrap();
        let cfg =
            Config::from_args(argv(&["--db", "t.redb", "--spec", p.to_str().unwrap()])).unwrap();
        let spec = cfg.spec.expect("--spec should parse to Some");
        assert_eq!(spec.equality.len(), 1);
        assert_eq!(spec.equality[0].label, "Person");
        assert_eq!(spec.equality[0].prop, "handle");
        assert!(spec.text.is_empty());
    }

    #[test]
    fn scope_shared_is_case_insensitive() {
        let cfg = Config::from_args(argv(&["--db", "t.redb", "--scope", "SHARED"])).unwrap();
        assert!(matches!(cfg.default_scope, Scope::Shared));
    }

    #[test]
    fn scope_ulid_parses_to_id_and_round_trips_label() {
        let id = ScopeId::new();
        let s = id.to_string();
        let cfg = Config::from_args(argv(&["--db", "t.redb", "--scope", &s])).unwrap();
        match cfg.default_scope {
            Scope::Id(got) => assert_eq!(got, id),
            other => panic!("expected Scope::Id, got {other:?}"),
        }
        assert_eq!(scope_label(&cfg.default_scope), s);
    }

    #[test]
    fn scope_label_shared() {
        assert_eq!(scope_label(&Scope::Shared), "shared");
    }

    #[test]
    fn bad_scope_is_rejected() {
        assert!(Config::from_args(argv(&["--db", "t.redb", "--scope", "not-a-ulid"])).is_err());
    }

    #[test]
    fn missing_db_is_rejected() {
        assert!(Config::from_args(argv(&["--scope", "shared"])).is_err());
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(Config::from_args(argv(&["--db", "t.redb", "--nope"])).is_err());
    }

    #[test]
    fn missing_value_is_rejected() {
        assert!(Config::from_args(argv(&["--db"])).is_err());
    }
}
