//! OKF markdown link extraction + resolution. OKF pages reference each other
//! with plain relative/absolute markdown links `[label](/path.md)` or
//! `[label](path.md)` — NOT obsidian `[[wikilinks]]` (design §"Why a new
//! crate", point 1). Node identity is the bundle-relative file path.

/// Extract every `[label](href)` markdown link in document order (no dedup;
/// image embeds `![...]()` are skipped).
pub fn extract_links(body: &str) -> Vec<(String, String)> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'[' || (i > 0 && bytes[i - 1] == b'!') {
            i += 1;
            continue;
        }
        // `[label]` then immediately `(href)`.
        let Some(close_label) = body[i + 1..].find(']') else {
            break;
        };
        let label_end = i + 1 + close_label;
        let after = &body[label_end + 1..];
        if !after.starts_with('(') {
            i = label_end + 1;
            continue;
        }
        let Some(close_href) = after[1..].find(')') else {
            i = label_end + 1;
            continue;
        };
        let href_start = label_end + 2;
        let href_end = href_start + close_href;
        let label = body[i + 1..label_end].to_string();
        let href = body[href_start..href_end].to_string();
        out.push((label, href));
        i = href_end + 1;
    }
    out
}

/// Resolve a body-link `href` to a bundle-relative path, given the source
/// page's directory (`""` for the root). A leading-slash href is
/// bundle-root-absolute; anything else is relative to `source_dir`. Returns
/// `None` for external links (scheme present) and non-`.md` targets.
pub fn resolve_link(source_dir: &str, href: &str) -> Option<String> {
    let href = href.split('#').next().unwrap_or(href);
    let href = href.split('?').next().unwrap_or(href);
    if href.is_empty() || href.contains("://") || href.contains(':') {
        // `mailto:`, `https://…`, etc. — not a bundle path.
        return None;
    }
    if !href.ends_with(".md") {
        return None;
    }
    let combined = if let Some(abs) = href.strip_prefix('/') {
        abs.to_string()
    } else if source_dir.is_empty() {
        href.to_string()
    } else {
        format!("{source_dir}/{href}")
    };
    Some(normalize_rel(&combined))
}

/// Collapse `.`/`..` segments into a clean bundle-relative path.
fn normalize_rel(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            c => stack.push(c),
        }
    }
    stack.join("/")
}
