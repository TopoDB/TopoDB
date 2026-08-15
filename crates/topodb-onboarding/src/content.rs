//! Canonical onboarding content: the conventions body and the per-client
//! pointer block. One source of truth (Principle: DRY across clients).

/// Bump when the conventions template or schedule defaults change. Gates the
/// global-scaffold fast-path and the fence `version=N`.
pub const ONBOARDING_VERSION: u32 = 1;

const CONVENTIONS_MD: &str = include_str!("../templates/CONVENTIONS.md");

/// The inner pointer text (no fence markers). ~5 lines; keep it short.
const POINTER_BODY: &str = "\
This project uses **TopoDB** for agent memory.
- Before writing memories, read `CONVENTIONS.md` (scope discipline, when to remember).
- Search memory before asking the user to repeat context you may already have.
- Store durable facts/decisions with `remember`; supersede when they change.";

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
    }

    #[test]
    fn conventions_has_version_header_and_core_rules() {
        let c = conventions_markdown();
        assert!(c.contains("version:"));
        assert!(c.to_lowercase().contains("scope"));
        assert!(c.to_lowercase().contains("remember"));
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
