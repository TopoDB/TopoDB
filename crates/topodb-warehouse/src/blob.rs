//! Content-addressed blob store for artifact bodies over `max_inline_kb`.
use crate::Layout;
use std::path::PathBuf;

/// `"b3:" + blake3 hex` — the warehouse's canonical content hash form.
pub fn hash_hex(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

/// `blobs/ab/cd/<full hash>` (fan-out on the first two hex byte pairs).
pub fn blob_path(layout: &Layout, hash: &str) -> PathBuf {
    let hex = hash.strip_prefix("b3:").unwrap_or(hash);
    let (a, b) = if hex.len() >= 4 {
        (&hex[0..2], &hex[2..4])
    } else {
        ("xx", "xx")
    };
    layout.blobs.join(a).join(b).join(hash.replace(':', "_"))
}

/// Writes once (tmp + rename); returns Ok(true) if written, Ok(false) if it existed.
pub fn put_blob(layout: &Layout, hash: &str, bytes: &[u8]) -> std::io::Result<bool> {
    let p = blob_path(layout, hash);
    if p.is_file() {
        return Ok(false);
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, &p) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if p.is_file() {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

pub fn get_blob(layout: &Layout, hash: &str) -> std::io::Result<Option<Vec<u8>>> {
    let p = blob_path(layout, hash);
    if !p.is_file() {
        return Ok(None);
    }
    std::fs::read(&p).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Layout;
    #[test]
    fn put_is_idempotent_and_get_roundtrips() {
        let t = tempfile::tempdir().unwrap();
        let l = Layout::new(t.path().join("w"));
        l.ensure().unwrap();
        let h = hash_hex(b"hello");
        assert!(h.starts_with("b3:"));
        assert!(put_blob(&l, &h, b"hello").unwrap()); // written
        assert!(!put_blob(&l, &h, b"hello").unwrap()); // already there
        assert_eq!(get_blob(&l, &h).unwrap().unwrap(), b"hello");
        assert!(get_blob(&l, "b3:nope").unwrap().is_none());
        assert!(blob_path(&l, &h).starts_with(l.blobs.join(&h[3..5]).join(&h[5..7])));
    }
}
