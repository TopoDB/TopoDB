//! CLI parsing and the server configuration contract.
//!
//! CLI: `topodb-mcp --db <path> [--scope <ulid|shared>]
//!      [--read-scopes <ulid|shared>[,...]] [--spec <spec.json>]
//!      [--allow-unscoped-changes]`
//! - `--scope`: the default **write** scope — the scope a created node/edge is
//!   stamped with when a write tool omits `scope`. `"shared"` (case-insensitive)
//!   or omitted => [`Scope::Shared`]; any other value is parsed as a ULID.
//! - `--read-scopes`: the default **read** scope set — the scopes a read tool
//!   filters by when it omits `scope`/`scopes`. Comma-separated. Defaults to
//!   just `--scope`'s value, which is the single-scope behaviour every existing
//!   client relies on. A read filters by a *set*; a write picks *one* scope —
//!   hence two flags rather than one overloaded flag.
//! - `--spec`: path to a JSON file deserializing to [`IndexSpec`], honored
//!   verbatim (may reindex an existing db). Omitted => inherit the db's
//!   persisted spec on an existing file, or create a fresh db with the
//!   [built-in default spec](default_spec). See how `main` opens the db.
//! - `--allow-unscoped-changes`: a bare toggle enabling `get_changes`, the one
//!   unscoped read (the op log spans every scope in the db). Off by default —
//!   in a db shared across projects, an agent calling `get_changes` would
//!   otherwise replay every other project's writes. Sync/consolidation hosts
//!   that legitimately need the whole log pass this flag.
//!
//! Arg parsing is hand-rolled: the surface is five flags, so `clap` would add
//! a dependency and a proc-macro build for no real gain here.

use std::error::Error;
use std::path::PathBuf;
use std::str::FromStr;

use topodb::{IndexSpec, Scope, ScopeId};

/// Label/prop name constants, single-sourced in `topodb-json` (shared with
/// `topodb-cli`'s `create-entity`/`create-memory`) and re-exported here so
/// existing `topodb-mcp` call sites (`use crate::config::{ENTITY_LABEL, ...}`)
/// keep working unchanged.
pub use topodb_json::{
    ALIAS_EDGE_TYPE, ALIAS_LABEL, ALIAS_NAME_PROP, ENTITY_LABEL, ENTITY_NAME_PROP,
    MEMORY_CONTENT_PROP, MEMORY_LABEL,
};

/// A **non-empty** set of scopes a read filters by. The non-empty invariant is
/// structural rather than conventional: an empty [`ScopeSet`] admits nothing, so
/// an empty read set would make every default read silently return empty. There
/// is no unscoped read, and "read nothing" is never what a caller means.
///
/// Distinct from [`Config::default_scope`], the single [`Scope`] a *write* is
/// stamped with. A read filters by a set; a write picks one.
///
/// [`ScopeSet`]: topodb::ScopeSet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadScopes(Vec<Scope>);

impl ReadScopes {
    /// Rejects an empty list. This is the only constructor.
    pub fn new(scopes: Vec<Scope>) -> Result<Self, Box<dyn Error>> {
        if scopes.is_empty() {
            return Err(
                "read scope set is empty; expected at least one of \"shared\" or a scope ULID"
                    .into(),
            );
        }
        Ok(Self(scopes))
    }

    /// The scopes, in the order given.
    pub fn as_slice(&self) -> &[Scope] {
        &self.0
    }
}

/// Resolved server configuration (see the module docs for the CLI contract).
#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub default_scope: Scope,
    /// The default read `ScopeSet`, as a non-empty list of scopes. Seeded from
    /// `--read-scopes`, or from `--scope` alone when that flag is omitted (so
    /// the single-scope behaviour every existing client relies on is preserved
    /// exactly). Distinct from `default_scope`, which is the single scope a
    /// *write* is stamped with — a read filters by a set, a write picks one.
    pub default_read_scopes: ReadScopes,
    /// The spec parsed from an explicit `--spec` file, or `None` when the flag
    /// was omitted. `None` means "inherit the db's persisted spec" (see how
    /// `main` opens the db), NOT "use `default_spec()`" — the two diverge for
    /// an existing db: silently substituting the default would reindex it and
    /// drop its declared equality indexes.
    pub spec: Option<IndexSpec>,
    /// Opt-in for `get_changes`, the one unscoped read — the op log spans every
    /// scope in the db, so in a db shared across projects it is a cross-project
    /// read of everything. Off unless the host explicitly asks for it. Sync and
    /// consolidation hosts, which legitimately need the whole log, pass
    /// `--allow-unscoped-changes`.
    pub allow_unscoped_changes: bool,
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

/// Parses a `--read-scopes` value: a comma-separated list of `shared` / ULID
/// entries, whitespace around each entry ignored. Rejects an empty list — an
/// empty `ScopeSet` admits nothing, and "read nothing" is never what a caller
/// means (there is no unscoped read).
fn parse_read_scopes(s: &str) -> Result<ReadScopes, Box<dyn Error>> {
    let scopes = s
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(parse_scope)
        .collect::<Result<Vec<_>, _>>()?;
    if scopes.is_empty() {
        return Err(format!(
            "--read-scopes value {s:?} is empty; expected a comma-separated list of \"shared\" or scope ULIDs"
        )
        .into());
    }
    ReadScopes::new(scopes)
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
        let mut read_scopes: Option<String> = None;
        let mut spec_path: Option<PathBuf> = None;
        let mut allow_unscoped_changes = false;

        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--db" => {
                    db_path = Some(it.next().ok_or("--db requires a <path> value")?.into());
                }
                "--scope" => {
                    scope = Some(it.next().ok_or("--scope requires a <ulid|shared> value")?);
                }
                "--read-scopes" => {
                    read_scopes = Some(
                        it.next()
                            .ok_or("--read-scopes requires a comma-separated <ulid|shared> list")?,
                    );
                }
                "--spec" => {
                    spec_path = Some(
                        it.next()
                            .ok_or("--spec requires a <spec.json> value")?
                            .into(),
                    );
                }
                "--allow-unscoped-changes" => {
                    allow_unscoped_changes = true;
                }
                other => {
                    return Err(format!(
                        "unknown argument {other:?}; usage: topodb-mcp --db <path> [--scope <ulid|shared>] [--read-scopes <ulid|shared>[,...]] [--spec <spec.json>] [--allow-unscoped-changes]"
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
        let default_read_scopes = match read_scopes {
            Some(s) => parse_read_scopes(&s)?,
            None => ReadScopes::new(vec![default_scope])?,
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
            default_read_scopes,
            spec,
            allow_unscoped_changes,
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

    #[test]
    fn read_scopes_defaults_to_the_write_scope() {
        let id = ScopeId::new();
        let s = id.to_string();
        let cfg = Config::from_args(argv(&["--db", "t.redb", "--scope", &s])).unwrap();
        assert_eq!(cfg.default_read_scopes.as_slice(), &[Scope::Id(id)]);
    }

    #[test]
    fn read_scopes_defaults_to_shared_when_scope_omitted() {
        let cfg = Config::from_args(argv(&["--db", "t.redb"])).unwrap();
        assert_eq!(cfg.default_read_scopes.as_slice(), &[Scope::Shared]);
    }

    #[test]
    fn read_scopes_parses_a_comma_separated_list() {
        let a = ScopeId::new();
        let list = format!("{a},shared");
        let cfg = Config::from_args(argv(&[
            "--db",
            "t.redb",
            "--scope",
            &a.to_string(),
            "--read-scopes",
            &list,
        ]))
        .unwrap();
        assert_eq!(
            cfg.default_read_scopes.as_slice(),
            &[Scope::Id(a), Scope::Shared]
        );
        // The write scope is untouched by --read-scopes.
        assert!(matches!(cfg.default_scope, Scope::Id(got) if got == a));
    }

    #[test]
    fn read_scopes_tolerates_whitespace_around_entries() {
        let a = ScopeId::new();
        let list = format!(" {a} , shared ");
        let cfg = Config::from_args(argv(&["--db", "t.redb", "--read-scopes", &list])).unwrap();
        assert_eq!(
            cfg.default_read_scopes.as_slice(),
            &[Scope::Id(a), Scope::Shared]
        );
    }

    #[test]
    fn read_scopes_rejects_a_bad_ulid() {
        assert!(Config::from_args(argv(&[
            "--db",
            "t.redb",
            "--read-scopes",
            "shared,not-a-ulid"
        ]))
        .is_err());
    }

    #[test]
    fn read_scopes_rejects_an_empty_list() {
        // An empty read set admits nothing — there is no unscoped read, so this is
        // a caller error, not "read everything".
        assert!(Config::from_args(argv(&["--db", "t.redb", "--read-scopes", ""])).is_err());
        assert!(Config::from_args(argv(&["--db", "t.redb", "--read-scopes", " , "])).is_err());
    }

    #[test]
    fn read_scopes_type_rejects_an_empty_set() {
        // The invariant is structural, not a parser convention: even a direct
        // construction cannot produce an empty read set. An empty ScopeSet
        // admits nothing, so every default read would silently return empty.
        assert!(ReadScopes::new(vec![]).is_err());
        assert!(ReadScopes::new(vec![Scope::Shared]).is_ok());
        assert!(ReadScopes::new(vec![Scope::Id(ScopeId::new()), Scope::Shared]).is_ok());
    }

    #[test]
    fn read_scopes_preserves_order_and_contents() {
        let a = ScopeId::new();
        let rs = ReadScopes::new(vec![Scope::Id(a), Scope::Shared]).unwrap();
        assert_eq!(rs.as_slice(), &[Scope::Id(a), Scope::Shared]);
    }

    #[test]
    fn unscoped_changes_is_off_by_default() {
        let cfg = Config::from_args(argv(&["--db", "t.redb"])).unwrap();
        assert!(!cfg.allow_unscoped_changes);
    }

    #[test]
    fn unscoped_changes_flag_is_a_bare_toggle() {
        let cfg = Config::from_args(argv(&["--db", "t.redb", "--allow-unscoped-changes"])).unwrap();
        assert!(cfg.allow_unscoped_changes);
    }
}
