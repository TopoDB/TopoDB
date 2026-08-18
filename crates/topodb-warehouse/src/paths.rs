//! Warehouse directory derivation + on-disk layout (spec §4).
use std::path::{Path, PathBuf};

/// `<db>.warehouse` next to the db file (`memory.redb` -> `memory.warehouse`),
/// unless `env_override` (`TOPODB_WAREHOUSE_DIR`) is set.
pub fn warehouse_dir_for_db(db_path: &Path, env_override: Option<&str>) -> PathBuf {
    if let Some(o) = env_override.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(o);
    }
    let mut p = db_path.to_path_buf();
    p.set_extension("warehouse");
    p
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    pub segments: PathBuf,
    pub archive: PathBuf,
    pub blobs: PathBuf,
    pub spool: PathBuf,
}

impl Layout {
    pub fn new(root: PathBuf) -> Self {
        Layout {
            segments: root.join("segments"),
            archive: root.join("archive"),
            blobs: root.join("blobs"),
            spool: root.join("spool"),
            root,
        }
    }
    pub fn ensure(&self) -> std::io::Result<()> {
        for p in [
            &self.root,
            &self.segments,
            &self.archive,
            &self.blobs,
            &self.spool,
        ] {
            std::fs::create_dir_all(p)?;
        }
        Ok(())
    }
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("MANIFEST.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn dir_is_sibling_with_warehouse_extension() {
        let d = warehouse_dir_for_db(Path::new("/x/y/memory.redb"), None);
        assert_eq!(d, Path::new("/x/y/memory.warehouse"));
    }
    #[test]
    fn dir_without_extension_appends() {
        let d = warehouse_dir_for_db(Path::new("/x/y/memory"), None);
        assert_eq!(d, Path::new("/x/y/memory.warehouse"));
    }
    #[test]
    fn env_override_wins() {
        let d = warehouse_dir_for_db(Path::new("/x/y/memory.redb"), Some("/elsewhere/wh"));
        assert_eq!(d, Path::new("/elsewhere/wh"));
    }
    #[test]
    fn layout_ensure_creates_all_dirs() {
        let t = tempfile::tempdir().unwrap();
        let l = Layout::new(t.path().join("w"));
        l.ensure().unwrap();
        for p in [&l.root, &l.segments, &l.archive, &l.blobs, &l.spool] {
            assert!(p.is_dir(), "{p:?}");
        }
    }
}
