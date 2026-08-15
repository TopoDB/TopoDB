//! Bundle walk + atomic note writes.

use crate::{Note, SKIP_FILES};
use std::fs;
use std::path::{Path, PathBuf};

/// Recursively collect concept-page candidates: every `.md` file except the
/// reserved `index.md`, the scratch/reserved [`SKIP_FILES`], and dotfiles
/// (design §Ingest step 1). Sorted for deterministic ingest order.
pub fn walk_bundle(dir: &Path) -> Result<Vec<PathBuf>, String> {
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
        } else if !name.starts_with('.')
            && name != "index.md"
            && !SKIP_FILES.contains(&name.as_ref())
            && path.extension().is_some_and(|e| e == "md")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Atomic write: same-dir tmp file + rename. Creates the parent directory.
pub fn write_note(path: &Path, note: &Note) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("md.topodb-tmp");
    fs::write(&tmp, note.serialize()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}
