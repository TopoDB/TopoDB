//! Reserved `index.md` generation. The root index carries the
//! `okf_version: "0.2"` bundle marker (OKF §8) and lists the emitted pages and
//! top-level directories; it is a pure, deterministic function of the seeded
//! file set (design §"Reserved files").

use crate::{Note, OKF_VERSION, OKF_VERSION_KEY};
use serde_yaml::{Mapping, Value as Yaml};
use std::collections::BTreeSet;

/// Build the root `index.md` from the emitted `(rel_path, description)` pairs.
pub fn render_root_index(emitted: &[(String, String)]) -> Note {
    let mut fm = Mapping::new();
    fm.insert(
        Yaml::String(OKF_VERSION_KEY.into()),
        Yaml::String(OKF_VERSION.into()),
    );

    let mut files: Vec<String> = Vec::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for (rel, description) in emitted {
        match rel.split_once('/') {
            Some((dir, _)) => {
                dirs.insert(dir.to_string());
            }
            None => {
                let label = rel.strip_suffix(".md").unwrap_or(rel);
                if description.is_empty() {
                    files.push(format!("- [{label}](/{rel})"));
                } else {
                    files.push(format!("- [{label}](/{rel}) — {description}"));
                }
            }
        }
    }
    files.sort();

    let mut body = String::from("# Files\n\n");
    for line in &files {
        body.push_str(line);
        body.push('\n');
    }
    body.push_str("\n# Directories\n\n");
    for dir in &dirs {
        body.push_str(&format!("- [{dir}](/{dir}/)\n"));
    }

    Note {
        frontmatter: fm,
        body,
    }
}
