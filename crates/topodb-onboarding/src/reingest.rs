//! Reingest: resolve configured sources to absolute paths, then re-ingest each
//! Obsidian vault / OKF bundle into the graph. Text-only (no embedder) in v1.

use std::path::{Path, PathBuf};

use topodb::{Db, Scope};

use crate::config::{ReingestSource, SourceKind};

/// A source with its `path` resolved to an absolute location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    pub kind: SourceKind,
    pub path: PathBuf,
    pub scope: Option<String>,
}

/// Resolve each source's `path` against `base_dir` (the directory of the
/// `.topodb.toml` that declared it): a leading `~`/`~/` expands to `home`
/// when provided; an absolute path passes through; a relative path joins
/// `base_dir`. `~` with `home = None` is left literal (the source then simply
/// fails to ingest and is reported). Pure — no env or filesystem access.
pub fn resolve_sources(
    base_dir: &Path,
    home: Option<&Path>,
    sources: &[ReingestSource],
) -> Vec<ResolvedSource> {
    sources
        .iter()
        .map(|s| ResolvedSource {
            kind: s.kind,
            path: resolve_one(base_dir, home, &s.path),
            scope: s.scope.clone(),
        })
        .collect()
}

fn resolve_one(base_dir: &Path, home: Option<&Path>, raw: &str) -> PathBuf {
    if let Some(h) = home {
        if raw == "~" {
            return h.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return h.join(rest);
        }
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// Per-file error surfaced by a reingest (normalized from the ingest crates'
/// `FileError`, plus whole-source failures where the entry point returned
/// `Err`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReingestFileError {
    pub file: String,
    pub reason: String,
}

/// Normalized outcome of re-ingesting one source. Mirrors the ingest crates'
/// `IngestReport`, tagged with which source produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReingestReport {
    pub kind: SourceKind,
    pub path: String,
    pub ingested: usize,
    pub superseded: usize,
    pub deduplicated: usize,
    pub skipped: usize,
    pub errors: Vec<ReingestFileError>,
}

/// Re-ingest every resolved source into `db`, text-only (no embedder). Each
/// source is attempted independently; a whole-source failure becomes a
/// single-error report rather than aborting the rest. `catch_up_scope` is the
/// default write scope when a source declares none (or its scope string fails
/// to parse).
pub fn run_reingest(
    db: &Db,
    sources: &[ResolvedSource],
    catch_up_scope: Scope,
    now_ms: i64,
) -> Vec<ReingestReport> {
    sources
        .iter()
        .map(|s| {
            let write_scope = topodb_json::resolve_scope(s.scope.as_deref(), catch_up_scope)
                .unwrap_or(catch_up_scope);
            let lookup = topodb_json::scope_to_scope_set(write_scope);
            let path = s.path.display().to_string();
            match s.kind {
                SourceKind::Obsidian => {
                    match topodb_obsidian::ingest_vault(
                        db, &s.path, write_scope, &lookup, now_ms, false, None,
                    ) {
                        Ok(r) => ReingestReport {
                            kind: s.kind,
                            path,
                            ingested: r.ingested,
                            superseded: r.superseded,
                            deduplicated: r.deduplicated,
                            skipped: r.skipped,
                            errors: r
                                .errors
                                .into_iter()
                                .map(|e| ReingestFileError { file: e.file, reason: e.reason })
                                .collect(),
                        },
                        Err(reason) => whole_source_error(s.kind, path, reason),
                    }
                }
                SourceKind::Okf => {
                    match topodb_okf::ingest_okf(
                        db, &s.path, write_scope, &lookup, now_ms, false, None,
                    ) {
                        Ok(r) => ReingestReport {
                            kind: s.kind,
                            path,
                            ingested: r.ingested,
                            superseded: r.superseded,
                            deduplicated: r.deduplicated,
                            skipped: r.skipped,
                            errors: r
                                .errors
                                .into_iter()
                                .map(|e| ReingestFileError { file: e.file, reason: e.reason })
                                .collect(),
                        },
                        Err(reason) => whole_source_error(s.kind, path, reason),
                    }
                }
            }
        })
        .collect()
}

fn whole_source_error(kind: SourceKind, path: String, reason: String) -> ReingestReport {
    ReingestReport {
        kind,
        path: path.clone(),
        ingested: 0,
        superseded: 0,
        deduplicated: 0,
        skipped: 0,
        errors: vec![ReingestFileError { file: path, reason }],
    }
}

/// The platform home directory from the environment (`HOME` on unix,
/// `USERPROFILE` on Windows), or `None` if unset.
pub fn env_home() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use topodb::{Db, Scope};

    fn src(kind: SourceKind, path: &str) -> ReingestSource {
        ReingestSource { kind, path: path.to_string(), scope: None }
    }

    fn open_db(dir: &std::path::Path) -> Db {
        Db::open_with(dir.join("m.redb"), topodb_json::default_spec()).unwrap()
    }

    fn write(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn reingests_obsidian_vault_then_is_a_fixpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        write(&vault.join("a.md"), "---\nkind: semantic\n---\nRedb is the storage engine.\n");
        let db = open_db(tmp.path());

        let resolved = vec![ResolvedSource {
            kind: SourceKind::Obsidian,
            path: vault.clone(),
            scope: None,
        }];
        let r1 = run_reingest(&db, &resolved, Scope::Shared, 1_000);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].kind, SourceKind::Obsidian);
        assert_eq!(r1[0].ingested, 1);
        assert!(r1[0].errors.is_empty());

        // Second pass over the (now id-stamped) unchanged vault: no new memory.
        let r2 = run_reingest(&db, &resolved, Scope::Shared, 2_000);
        assert_eq!(r2[0].ingested, 0);
    }

    #[test]
    fn missing_path_is_a_reported_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_db(tmp.path());
        let resolved = vec![ResolvedSource {
            kind: SourceKind::Okf,
            path: tmp.path().join("does-not-exist"),
            scope: None,
        }];
        let r = run_reingest(&db, &resolved, Scope::Shared, 1_000);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].ingested, 0);
        assert_eq!(r[0].errors.len(), 1);
    }

    #[test]
    fn resolves_relative_against_base_dir() {
        let out = resolve_sources(
            Path::new("/proj"),
            Some(Path::new("/home/u")),
            &[src(SourceKind::Okf, "./knowledge")],
        );
        assert_eq!(out[0].path, PathBuf::from("/proj/./knowledge"));
    }

    #[test]
    fn expands_tilde_to_home() {
        let out = resolve_sources(
            Path::new("/proj"),
            Some(Path::new("/home/u")),
            &[src(SourceKind::Obsidian, "~/notes/vault")],
        );
        assert_eq!(out[0].path, PathBuf::from("/home/u/notes/vault"));
    }

    #[test]
    fn absolute_passes_through_and_tilde_literal_without_home() {
        let out = resolve_sources(
            Path::new("/proj"),
            None,
            &[src(SourceKind::Okf, "/abs/bundle"), src(SourceKind::Okf, "~/x")],
        );
        assert_eq!(out[0].path, PathBuf::from("/abs/bundle"));
        assert_eq!(out[1].path, PathBuf::from("/proj/~/x")); // ~ left literal, joined to base
    }
}
