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
}
