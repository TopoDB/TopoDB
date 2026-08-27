//! Canonical onboarding content: the conventions body and the per-client
//! pointer block. One source of truth (Principle: DRY across clients).

/// Bump when the conventions template or schedule defaults change. Gates the
/// global-scaffold fast-path and the fence `version=N`.
///
/// Coupled to `templates/CONVENTIONS.md`'s own `version:` header only by
/// convention, not by the type system — bump BOTH together. See
/// `template_version_matches_onboarding_version` below, which pins them.
pub const ONBOARDING_VERSION: u32 = 2;

const CONVENTIONS_MD: &str = include_str!("../templates/CONVENTIONS.md");

/// The inner pointer text (no fence markers). ~5 lines; keep it short.
const POINTER_BODY: &str = "\
This project uses **TopoDB** for agent memory.
- Before writing memories, read `CONVENTIONS.md` (what to store, when to merge, when to retire).
- Search memory before asking the user to repeat context you may already have.
- Store durable facts/decisions with `remember`; supersede when they change.
- Act on `memory_health`; scans never delete.";

pub fn conventions_markdown() -> &'static str {
    CONVENTIONS_MD
}

pub fn pointer_body() -> &'static str {
    POINTER_BODY
}

pub fn pointer_block() -> String {
    format!(
        "<!-- topodb:pointer:start version={v} -->\n{body}\n<!-- topodb:pointer:end -->\n",
        v = ONBOARDING_VERSION,
        body = POINTER_BODY,
    )
}

/// Parses the leading `version: N` line emitted at the top of
/// `CONVENTIONS.md` (both the template and whatever is already on disk).
fn parse_version_line(text: &str) -> Option<u32> {
    text.lines().find_map(|l| {
        l.trim()
            .strip_prefix("version:")
            .and_then(|rest| rest.trim().parse::<u32>().ok())
    })
}

/// Writes `<dir>/CONVENTIONS.md` from [`conventions_markdown`] if the file is
/// missing, or if its existing `version:` header is older than the current
/// template's (`ONBOARDING_VERSION` — the template's own header always
/// matches it). Returns `Ok(true)` if it wrote, `Ok(false)` if an
/// up-to-date file was left untouched.
///
/// One copy shared by every onboarding entrypoint (`topodb init`, MCP
/// server boot, ...) so the "write if missing/older" policy can't drift
/// between callers.
pub fn ensure_conventions_file(dir: &std::path::Path) -> std::io::Result<bool> {
    let path = dir.join("CONVENTIONS.md");
    let template = conventions_markdown();
    let template_version = parse_version_line(template).unwrap_or(ONBOARDING_VERSION);

    let existing_version = match std::fs::read_to_string(&path) {
        Ok(t) => parse_version_line(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };

    let needs_write = match existing_version {
        None => true,
        Some(v) => v < template_version,
    };
    if needs_write {
        std::fs::write(&path, template)?;
    }
    Ok(needs_write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_block_wraps_body_with_versioned_fence() {
        let b = pointer_block();
        assert!(b.starts_with(&format!(
            "<!-- topodb:pointer:start version={} -->",
            ONBOARDING_VERSION
        )));
        assert!(b.trim_end().ends_with("<!-- topodb:pointer:end -->"));
        assert!(b.contains(pointer_body().trim()));
        assert!(pointer_body().contains("when to merge"));
        assert!(pointer_body().contains("memory_health"));
    }

    /// `ensure_conventions_file` decides whether to rewrite CONVENTIONS.md by
    /// parsing the TEMPLATE's own `version:` header, not by reading
    /// `ONBOARDING_VERSION` directly (see `template_version = parse_version_line(template)`
    /// above, falling back to `ONBOARDING_VERSION` only if the header is
    /// missing/malformed). That fallback means a header/const mismatch would
    /// silently NOT fail loudly at that call site — it would just make
    /// refresh compare against the wrong number. Pin the two together here so
    /// bumping one without the other fails a test instead of failing silently
    /// in the field.
    #[test]
    fn template_version_matches_onboarding_version() {
        let template_version = parse_version_line(conventions_markdown())
            .expect("CONVENTIONS.md template must have a parsable `version:` header");
        assert_eq!(
            template_version, ONBOARDING_VERSION,
            "templates/CONVENTIONS.md's `version:` header must match ONBOARDING_VERSION \
             in content.rs — bump both together"
        );
    }

    #[test]
    fn conventions_has_version_header_and_core_rules() {
        let c = conventions_markdown();
        assert!(c.contains("version:"));
        assert!(c.to_lowercase().contains("scope"));
        assert!(c.to_lowercase().contains("remember"));
        assert!(c.contains("consolidate_memories"));
        assert!(c.contains("forget"));
        assert!(c.contains("memory_health"));
        assert!(c.contains("supersession_candidates"));
    }

    #[test]
    fn root_readme_surfaces_session_and_policy() {
        let readme = include_str!("../../../README.md");
        assert!(readme.contains("## A session"));
        assert!(readme.contains("## What an agent should remember"));
        assert!(readme.contains("consolidate_memories"));
        assert!(readme.contains("forget"));
        assert!(readme.contains("memory_health"));
        assert!(readme.contains("supersession_candidates"));
    }

    #[test]
    fn ensure_conventions_file_writes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let wrote = ensure_conventions_file(dir.path()).unwrap();
        assert!(wrote);
        let on_disk = std::fs::read_to_string(dir.path().join("CONVENTIONS.md")).unwrap();
        assert_eq!(on_disk, conventions_markdown());
    }

    #[test]
    fn ensure_conventions_file_leaves_up_to_date_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CONVENTIONS.md"), conventions_markdown()).unwrap();
        let wrote = ensure_conventions_file(dir.path()).unwrap();
        assert!(!wrote);
    }

    #[test]
    fn ensure_conventions_file_rewrites_older_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CONVENTIONS.md"),
            "version: 0\nstale content\n",
        )
        .unwrap();
        let wrote = ensure_conventions_file(dir.path()).unwrap();
        assert!(wrote);
        let on_disk = std::fs::read_to_string(dir.path().join("CONVENTIONS.md")).unwrap();
        assert_eq!(on_disk, conventions_markdown());
    }
}
