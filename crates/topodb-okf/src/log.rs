//! Reserved `log.md` generation (OKF §9). TopoDB owns a temporal history, so a
//! change log is nearly free to emit — the reserved file a foreign producer
//! finds expensive to synthesize. Gated behind `with_log` on seed.
//!
//! This deterministic minimal form lists the emitted pages; a richer
//! date-grouped history from valid-time supersession is a recorded follow-up.

use crate::Note;
use serde_yaml::Mapping;

pub fn render_log(emitted: &[(String, String)]) -> Note {
    let mut body = String::from("# Log\n\n");
    let mut lines: Vec<String> = emitted
        .iter()
        .map(|(rel, description)| {
            let label = rel.strip_suffix(".md").unwrap_or(rel);
            if description.is_empty() {
                format!("- [{label}](/{rel})")
            } else {
                format!("- [{label}](/{rel}) — {description}")
            }
        })
        .collect();
    lines.sort();
    for line in lines {
        body.push_str(&line);
        body.push('\n');
    }
    Note {
        frontmatter: Mapping::new(),
        body,
    }
}
