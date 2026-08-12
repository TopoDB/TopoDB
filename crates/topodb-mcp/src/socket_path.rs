//! Derivation of the TopoDB daemon IPC endpoint path from a resolved DB path.
//!
//! The resident daemon (`topodb-mcp --socket`) listens on a per-DB, per-user IPC
//! endpoint so that one process — the redb lock holder — can multiplex every
//! front end (plugin sessions, CLI calls, hooks) onto the shared `Db`. This
//! module is the single source of truth for *where* that endpoint lives, shared
//! by the daemon (which binds it) and every client (which connects to it).
//!
//! # Matching the JS front end
//!
//! `plugins/claude-code/ipc.js` (`socketPathFor`) derives the same endpoint for
//! the plugin's shim/broker today, as:
//!
//! ```js
//! createHash("sha256").update(path.resolve(dbPath), "utf8").digest("hex").slice(0, 12)
//! ```
//!
//! We reuse that **sha-of-path scheme byte-for-byte** — SHA-256 of the resolved
//! path's UTF-8 bytes, lowercase hex, first 12 characters — so the Rust daemon
//! and the JS launcher agree on the socket for a given database. The caller is
//! responsible for passing an already-resolved (absolute, normalized) path, the
//! way `path.resolve` produces one on the JS side.
//!
//! # Two deliberate differences from `ipc.js`
//!
//! 1. **Per-user location with owner-only permissions.** `ipc.js` drops the
//!    socket straight in the shared OS temp dir. The daemon serves reads and
//!    writes for potentially many projects, so its socket lives in a per-user
//!    runtime directory (`$XDG_RUNTIME_DIR` when present — already `0700` and
//!    per-user — otherwise a per-user subdirectory of the temp dir that
//!    [`ensure_socket_dir`] creates `0700`). No other user can connect.
//! 2. **Protocol/version tag in the name.** The socket file/pipe name embeds
//!    [`PROTOCOL_TAG`]. When the wire protocol changes incompatibly we bump
//!    [`PROTOCOL_VERSION`]; an old client then looks for the old name, does not
//!    find it, and falls back to direct-open rather than handshaking with a
//!    daemon it cannot speak to.
//!
//! Both concerns keep the tmp-path-length lesson from `ipc.js`: the endpoint is
//! *never* placed beside the database (a `sun_path` is limited to ~104 bytes and
//! a DB path can be arbitrarily deep), so the short hashed name is what binds.

use std::path::{Path, PathBuf};

/// Wire-protocol version. Bump on any incompatible change to the daemon's
/// framing/handshake so old clients and new daemons never pair up.
pub const PROTOCOL_VERSION: u32 = 1;

/// Human-readable tag embedded in the endpoint name, e.g. `v1`. Derived from
/// [`PROTOCOL_VERSION`] via [`protocol_tag`] — kept as a runtime string so the
/// name is built in one place.
#[allow(dead_code)] // asserted in tests as a version-drift guard
pub const PROTOCOL_TAG: &str = "v1";

/// The `v{N}` tag embedded in endpoint names. Asserted equal to
/// [`PROTOCOL_TAG`] in tests, so a version bump that forgets the tag is caught.
pub fn protocol_tag() -> String {
    format!("v{PROTOCOL_VERSION}")
}

/// The basename shared by both platforms: `topodb-v{N}-{hash12}`. Unix appends
/// `.sock`; Windows prefixes the pipe namespace.
fn endpoint_stem(db_path: &Path) -> String {
    format!(
        "topodb-{}-{}",
        protocol_tag(),
        hash12(&db_path.to_string_lossy())
    )
}

/// Derive the daemon IPC endpoint path for a resolved database path.
///
/// Pure: same input always yields the same output, and it touches nothing on
/// disk. Call [`ensure_socket_dir`] before binding to guarantee the parent
/// directory exists with owner-only permissions.
///
/// - **Unix:** a unix-domain-socket path,
///   `{runtime_dir}/topodb-v{N}-{hash12}.sock`.
/// - **Windows:** a named-pipe name, `\\.\pipe\topodb-v{N}-{hash12}`.
pub fn socket_path_for(db_path: &Path) -> PathBuf {
    let stem = endpoint_stem(db_path);
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\{stem}"))
    }
    #[cfg(not(windows))]
    {
        runtime_dir().join(format!("{stem}.sock"))
    }
}

/// Ensure the endpoint's parent directory exists with owner-only permissions.
///
/// On unix, creates the runtime directory (if absent) and tightens it to `0700`
/// so no other user can reach the socket. On Windows the endpoint is a named
/// pipe with no filesystem parent, so this is a no-op (pipe ACLs are set at
/// bind time by the daemon, not here).
pub fn ensure_socket_dir(endpoint: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        if let Some(dir) = endpoint.parent() {
            let preexisting = dir.exists();
            std::fs::create_dir_all(dir)?;
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
                // A directory WE created must be lockable — propagate. One we
                // did not create may legitimately refuse chmod while already
                // being per-user-private ($XDG_RUNTIME_DIR, macOS's /var/folders
                // temp dir, an explicit --socket location): the caller chose it,
                // so its permissions are the caller's responsibility.
                if !preexisting {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
        Ok(())
    }
}

/// Per-user runtime directory for daemon sockets on unix.
///
/// Prefers `$XDG_RUNTIME_DIR` (systemd guarantees it is per-user and `0700`).
/// Otherwise falls back to a per-user subdirectory of the temp dir —
/// `{temp}/topodb-{user}` — which [`ensure_socket_dir`] locks to `0700`. macOS's
/// temp dir is already a private per-user directory, so the subdir is belt-and-
/// suspenders there and load-bearing on a shared Linux `/tmp`.
#[cfg(not(windows))]
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let user = std::env::var_os("USER")
        .or_else(|| std::env::var_os("LOGNAME"))
        .map(|u| u.to_string_lossy().into_owned())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "default".to_string());
    // Keep the discriminator filesystem-safe: a hostile $USER cannot escape the
    // temp dir via path separators.
    let user: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("topodb-{user}"))
}

/// SHA-256 of `input`'s UTF-8 bytes, lowercase hex, truncated to 12 characters.
///
/// Byte-for-byte the scheme `plugins/claude-code/ipc.js` uses
/// (`createHash("sha256").update(s, "utf8").digest("hex").slice(0, 12)`), so
/// both languages resolve the same DB path to the same endpoint name.
fn hash12(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(12);
    // 12 hex chars == the first 6 bytes of the digest.
    for byte in &digest[..6] {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_tag_tracks_version() {
        // A version bump that forgets to update the tag is a version-skew bug.
        assert_eq!(protocol_tag(), PROTOCOL_TAG);
        assert_eq!(PROTOCOL_TAG, format!("v{PROTOCOL_VERSION}"));
    }

    #[test]
    fn hash12_matches_canonical_sha256_vectors() {
        // Canonical SHA-256 known-answer vectors, truncated to 12 hex chars.
        // These pin our hashing to exactly what ipc.js computes for the same
        // input string: sha256 -> hex -> slice(0, 12).
        assert_eq!(hash12(""), "e3b0c44298fc");
        assert_eq!(hash12("abc"), "ba7816bf8f01");
    }

    #[test]
    fn hash12_is_twelve_lowercase_hex_chars() {
        let h = hash12("/Users/drew/.topodb/memory.redb");
        assert_eq!(h.len(), 12);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    // Compare on file_name, not the full path: the directory depends on process
    // env (XDG_RUNTIME_DIR), which a sibling test may flip mid-run under cargo's
    // parallel test harness. The hashed name is what has to be stable/distinct.
    fn endpoint_name(db: &str) -> String {
        socket_path_for(Path::new(db))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn derivation_is_deterministic() {
        assert_eq!(
            endpoint_name("/tmp/topodb/memory.redb"),
            endpoint_name("/tmp/topodb/memory.redb")
        );
    }

    #[test]
    fn distinct_db_paths_get_distinct_endpoints() {
        assert_ne!(endpoint_name("/tmp/a.redb"), endpoint_name("/tmp/b.redb"));
    }

    #[test]
    fn endpoint_name_embeds_version_tag_and_hash() {
        let name = socket_path_for(Path::new("/tmp/test.redb"))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let expected_hash = hash12("/tmp/test.redb");
        assert!(name.contains(PROTOCOL_TAG), "missing version tag in {name}");
        assert!(name.contains(&expected_hash), "missing hash in {name}");
        assert!(name.contains(&format!("topodb-{PROTOCOL_TAG}-{expected_hash}")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_yields_named_pipe() {
        let p = socket_path_for(Path::new(r"C:\Users\me\memory.redb"));
        let s = p.to_string_lossy();
        assert!(s.starts_with(r"\\.\pipe\topodb-"), "not a named pipe: {s}");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_yields_dot_sock_in_a_directory() {
        let p = socket_path_for(Path::new("/tmp/test.redb"));
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("sock"));
        assert!(p.parent().is_some(), "socket has no parent directory");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_runtime_dir_prefers_xdg_then_falls_back_to_per_user_temp() {
        // Both directions are asserted in ONE test so the env mutation is not
        // observed by a concurrently-running sibling. runtime_dir() is the only
        // thing that reads XDG_RUNTIME_DIR, and it is called synchronously here.
        let saved = std::env::var_os("XDG_RUNTIME_DIR");

        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        let with_xdg = socket_path_for(Path::new("/tmp/test.redb"));
        assert!(
            with_xdg.starts_with("/run/user/4242"),
            "expected socket under XDG_RUNTIME_DIR, got {}",
            with_xdg.display()
        );

        std::env::remove_var("XDG_RUNTIME_DIR");
        let without_xdg = socket_path_for(Path::new("/tmp/test.redb"));
        let parent = without_xdg
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(
            parent.starts_with("topodb-"),
            "expected per-user topodb-* subdir, got {parent}"
        );

        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }
}
