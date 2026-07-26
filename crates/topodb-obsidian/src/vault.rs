use crate::Note;
use std::fs;
use std::path::{Path, PathBuf};

pub fn walk_vault(dir: &Path) -> Result<Vec<PathBuf>, String> {
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let mut out = Vec::new();
    walk_into(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_into(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !name.starts_with('.') {
                walk_into(&path, out)?;
            }
        } else if !name.starts_with('.') && path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Atomic write: same-dir tmp file + rename (rename replaces on all
/// supported platforms; Windows EPERM transients are a known CI class).
pub fn write_note(path: &Path, note: &Note) -> Result<(), String> {
    let tmp = path.with_extension("md.topodb-tmp");
    fs::write(&tmp, note.serialize()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

pub fn stamp_id(path: &Path, note: &mut Note, id: &str) -> Result<(), String> {
    note.set_id(id);
    write_note(path, note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vault"))
    }

    #[test]
    fn walk_finds_md_skips_dot_dirs_and_non_md() {
        let names: Vec<String> = walk_vault(fixture())
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "bad-yaml.md",
                "decision-auth.md",
                "nested-note.md",
                "plain-note.md"
            ]
        ); // full-path sort: "notes/nested-note.md" < "plain-note.md"
        assert!(walk_vault(&fixture().join("missing")).is_err());
    }

    #[test]
    fn write_note_is_atomic_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("n.md");
        let mut note = crate::Note::parse("---\nkind: note\n---\nbody\n").unwrap();
        write_note(&p, &note).unwrap();
        stamp_id(&p, &mut note, "01ZZZ").unwrap();
        let back = crate::Note::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(back.id().as_deref(), Some("01ZZZ"));
        assert_eq!(back.body, "body\n");
        assert!(!dir.path().join("n.md.topodb-tmp").exists());
    }
}
