//! Reingest: resolve configured sources to absolute paths, then re-ingest each
//! Obsidian vault / OKF bundle into the graph. Text-only (no embedder) in v1.

use std::path::{Path, PathBuf};

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

/// The platform home directory from the environment (`HOME` on unix,
/// `USERPROFILE` on Windows), or `None` if unset.
pub fn env_home() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(kind: SourceKind, path: &str) -> ReingestSource {
        ReingestSource { kind, path: path.to_string(), scope: None }
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
