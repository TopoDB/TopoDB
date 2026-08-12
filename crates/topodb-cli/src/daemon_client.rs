//! Socket-first daemon client for `topodb-cli`.
//!
//! Per the daemon design (see `docs/superpowers/specs/2026-08-12-topodb-daemon-
//! design.md`, "Client paths > CLI"), a CLI invocation prefers a resident
//! `topodb-mcp --socket` daemon over opening the `.redb` file itself:
//!
//! 1. **Discover** whether a daemon socket exists for the resolved DB path.
//! 2. **If present** → connect, send a `topodb/hello` frame carrying this
//!    invocation's scope flags, issue the CLI command as the equivalent MCP
//!    tool call, and unwrap the tool result to the SAME JSON the direct
//!    in-process path prints (both front ends sit on `topodb-json`, so the
//!    payload shapes already match).
//! 3. **If absent** → signal [`Discovery::NoSocket`], the caller's cue to fall
//!    through to today's direct in-process open. This module NEVER spawns a
//!    daemon — a lone one-shot CLI call must not pay daemon-spawn cost, and
//!    spawn races are settled by redb-lock election on the daemon side, not
//!    here (`topodb daemon start` is the explicit opt-in for that regime).
//!
//! When a direct open instead exhausts its `Busy` budget and no socket was
//! found, [`BusyDiagnosis`] turns the bare "busy" JSON into a message naming
//! the likely *non-daemon* holder classes (an old pre-daemon binary, an `sgh`
//! run holding its lock-as-run-mutex lease, or a language binding), so the
//! operator knows who to look for.
//!
//! # Endpoint derivation — reused, not imported
//!
//! The daemon's endpoint path lives in `crates/topodb-mcp/src/socket_path.rs`
//! (`socket_path_for`, protocol tag `v1`). `topodb-mcp` is a heavy tokio+rmcp
//! crate and is deliberately NOT a dependency of this CLI; instead we reuse the
//! *same derivation scheme* byte-for-byte (that is why `sha2` is a CLI
//! dependency). [`socket_path_for`] here MUST stay identical to the daemon's so
//! both processes resolve a given DB path to the same endpoint — the hash
//! vectors pinned in the tests are the cross-crate contract.

use std::path::{Path, PathBuf};

/// Wire-protocol version. Bump in lockstep with `topodb-mcp`'s
/// `socket_path::PROTOCOL_VERSION` on any incompatible daemon framing change,
/// so an old client looks for the old name, fails to find it, and falls back to
/// direct open rather than handshaking with a daemon it cannot speak to.
pub const PROTOCOL_VERSION: u32 = 1;

/// Human-readable `v{N}` tag embedded in the endpoint name. Mirrors
/// `topodb-mcp`'s `socket_path::PROTOCOL_TAG`.
#[allow(dead_code)] // asserted in tests as a version-drift guard
pub const PROTOCOL_TAG: &str = "v1";

/// The `topodb/hello` frame key — the first line a client sends after
/// connecting, carrying this session's scopes. MUST match `HELLO_KEY` in
/// `plugins/claude-code/ipc.js` and the daemon's hello handling.
pub const HELLO_KEY: &str = "topodb/hello";

/// The `v{N}` tag embedded in endpoint names, derived from
/// [`PROTOCOL_VERSION`]. Asserted equal to [`PROTOCOL_TAG`] in tests.
pub fn protocol_tag() -> String {
    format!("v{PROTOCOL_VERSION}")
}

/// The basename shared by both platforms: `topodb-v{N}-{hash12}`. Unix appends
/// `.sock`; Windows prefixes the pipe namespace. Identical to
/// `topodb-mcp`'s `socket_path::endpoint_stem`.
fn endpoint_stem(db_path: &Path) -> String {
    format!(
        "topodb-{}-{}",
        protocol_tag(),
        hash12(&db_path.to_string_lossy())
    )
}

/// Derive the daemon IPC endpoint path for a resolved database path.
///
/// Pure and touches nothing on disk. This is the CLI-side twin of
/// `topodb-mcp`'s `socket_path::socket_path_for` and MUST yield the identical
/// path for the same input (the daemon binds it; this client connects to it).
///
/// - **Unix:** `{runtime_dir}/topodb-v{N}-{hash12}.sock`.
/// - **Windows:** the named pipe `\\.\pipe\topodb-v{N}-{hash12}`.
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

/// Per-user runtime directory for daemon sockets on unix. Prefers
/// `$XDG_RUNTIME_DIR` (already per-user and `0700`); otherwise a per-user
/// subdirectory of the temp dir. Identical to `topodb-mcp`'s
/// `socket_path::runtime_dir`, so both sides agree on the directory too.
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
/// Byte-for-byte the scheme `plugins/claude-code/ipc.js` and
/// `topodb-mcp`'s `socket_path::hash12` use, so all three resolve the same DB
/// path to the same endpoint name.
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

/// The scopes a `topodb/hello` frame carries: the CLI's write scope plus its
/// read set, so the daemon stamps this connection's requests correctly (one
/// daemon serves many projects against one shared DB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloScopes {
    /// The single write scope for this connection (`"shared"` or a ULID).
    pub scope: String,
    /// The read set (typically `[scope, "shared"]`).
    pub read_scopes: Vec<String>,
}

/// Serialize a [`HelloScopes`] into the newline-terminated hello frame the
/// daemon expects. MUST match `helloFrame` in `plugins/claude-code/ipc.js`:
/// `{"topodb/hello":{"scope":..,"read_scopes":[..]}}\n`.
pub fn hello_frame(scopes: &HelloScopes) -> String {
    let body = serde_json::json!({
        HELLO_KEY: {
            "scope": scopes.scope,
            "read_scopes": scopes.read_scopes,
        }
    });
    // Compact + trailing newline: newline-delimited JSON framing, same as MCP
    // over stdio, so the daemon reads exactly one frame.
    format!("{body}\n")
}

/// The outcome of socket discovery for a resolved DB path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// A daemon socket exists at this endpoint — the caller should connect.
    /// Presence, not liveness: a present-but-dead socket still fails at
    /// connect time, which surfaces as a [`DaemonError::Transport`].
    SocketPresent(PathBuf),
    /// No socket for this DB path — the caller falls through to a direct
    /// in-process open. This is the fall-through signal; this module never
    /// spawns a daemon to change it.
    NoSocket,
}

/// Whether a daemon socket file is present at `endpoint`.
///
/// A pure filesystem probe on unix (existence of the `.sock` inode). Split out
/// from [`discover`] so discovery is unit-testable without touching process
/// env: a test points it at a tempdir path it does or does not create.
///
/// On Windows the endpoint is a named pipe with no filesystem inode to `stat`,
/// and this crate has no std named-pipe client, so presence is reported
/// `false` (fall through to direct open) until real named-pipe support lands.
pub fn socket_present_at(endpoint: &Path) -> bool {
    #[cfg(not(windows))]
    {
        endpoint.exists()
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
        false
    }
}

/// Whether a daemon is actually LISTENING at `endpoint` — a real connect
/// probe, not a file-existence check. Distinguishes a live daemon from the
/// stale `.sock` inode a crashed daemon leaves behind (Rust never auto-unlinks
/// a unix socket). The probe connects and immediately drops, so it does not
/// complete a hello and is not counted as a session.
///
/// Windows: no std named-pipe client here, so reported `false` until named-pipe
/// support lands (same limitation as [`socket_present_at`]).
pub fn probe_live(endpoint: &Path) -> bool {
    #[cfg(not(windows))]
    {
        std::os::unix::net::UnixStream::connect(endpoint).is_ok()
    }
    #[cfg(windows)]
    {
        let _ = endpoint;
        false
    }
}

/// Discover whether a daemon serves the resolved DB path, without connecting.
/// [`Discovery::SocketPresent`] means "try to connect"; [`Discovery::NoSocket`]
/// is the cue to fall through to direct open.
pub fn discover(db_path: &Path) -> Discovery {
    let endpoint = socket_path_for(db_path);
    if socket_present_at(&endpoint) {
        Discovery::SocketPresent(endpoint)
    } else {
        Discovery::NoSocket
    }
}

/// An error talking to the daemon. Both variants are recoverable by the caller
/// re-running discovery on the next invocation; neither is a reason to spawn a
/// daemon here.
#[derive(Debug)]
pub enum DaemonError {
    /// Transport-level failure: connect refused (stale socket), broken pipe,
    /// read timeout, or a malformed/short frame. The socket looked present but
    /// could not be used.
    Transport(String),
    /// The daemon spoke MCP but returned an error: an `isError` tool result, a
    /// JSON-RPC error object, or an unexpected response shape.
    Protocol(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Transport(m) => write!(f, "daemon transport error: {m}"),
            DaemonError::Protocol(m) => write!(f, "daemon protocol error: {m}"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Unwrap an MCP `tools/call` result object into the inner JSON the direct CLI
/// path would print.
///
/// The daemon and the direct path both build their payloads with `topodb-json`,
/// so the unwrapped value matches shape-for-shape. Preference order:
/// 1. `isError: true` → a [`DaemonError::Protocol`] carrying the error text.
/// 2. `structuredContent` (present and non-null) → returned as-is; the daemon
///    puts the exact JSON body here.
/// 3. otherwise the first `content` block of `type: "text"`, parsed as JSON.
///
/// Anything else (no content, non-JSON text) is a [`DaemonError::Protocol`] —
/// a daemon that answered but not in the shared shape.
pub fn unwrap_tool_result(result: &serde_json::Value) -> Result<serde_json::Value, DaemonError> {
    let obj = result
        .as_object()
        .ok_or_else(|| DaemonError::Protocol("tool result was not a JSON object".to_string()))?;

    if obj.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        let text = obj
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| {
                a.iter()
                    .find_map(|b| b.get("text").and_then(|t| t.as_str()))
            })
            .unwrap_or("tool call reported isError with no message");
        return Err(DaemonError::Protocol(text.to_string()));
    }

    if let Some(structured) = obj.get("structuredContent") {
        if !structured.is_null() {
            return Ok(structured.clone());
        }
    }

    let text = obj
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| {
            a.iter().find_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| {
            DaemonError::Protocol(
                "tool result had no structuredContent or text content".to_string(),
            )
        })?;

    serde_json::from_str(text)
        .map_err(|e| DaemonError::Protocol(format!("tool result text was not JSON: {e}")))
}

/// A diagnosis for a `Busy` exhaustion: the CLI opened the DB directly, retried
/// for its whole `lock_wait_ms` budget, and still lost the flock.
///
/// The point of the classifier is to turn the bare `{"error":{"kind":"busy"}}`
/// into a message naming the *likely holder*. When `socket_present` is false,
/// the holder is by construction a **non-daemon** process — a daemon would have
/// been reached in the socket-first path and this contention would never have
/// happened — so we enumerate the non-daemon holder classes the design calls
/// out. When `socket_present` is true yet a direct open was still attempted
/// (e.g. a version-skew socket the client could not use), the wording points at
/// that instead.
#[derive(Debug, Clone)]
pub struct BusyDiagnosis {
    /// The DB path that could not be opened.
    pub db_path: PathBuf,
    /// The busy-retry budget that was exhausted, in milliseconds.
    pub lock_wait_ms: u64,
    /// Whether a daemon socket was found for this DB path.
    pub socket_present: bool,
}

/// The likely holder class of the flock when a direct open exhausts `Busy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LikelyHolder {
    /// A daemon socket exists but was unusable (version skew / failed
    /// election), and a direct open then contended with whatever holds the
    /// lock. The remedy is to inspect the daemon, not to hunt a stray process.
    UnusableDaemonSocket,
    /// No socket exists, so the holder is a non-daemon process — one of:
    /// an old pre-daemon `topodb` binary, an `sgh` run holding its
    /// lock-as-run-mutex lease, or a language binding that opened the file
    /// directly. The remedy is to find and stop that process (or
    /// `topodb daemon start` to move to the shared regime).
    NonDaemonProcess,
}

impl BusyDiagnosis {
    /// Classify the likely holder from what discovery found.
    pub fn likely_holder(&self) -> LikelyHolder {
        if self.socket_present {
            LikelyHolder::UnusableDaemonSocket
        } else {
            LikelyHolder::NonDaemonProcess
        }
    }

    /// A human-readable, exit-3 message that names the likely holder class.
    /// Callers pass this to `output::fail("busy", .., 3)` in place of the bare
    /// busy string; the exit code is unchanged.
    pub fn message(&self) -> String {
        let path = self.db_path.display();
        let ms = self.lock_wait_ms;
        match self.likely_holder() {
            LikelyHolder::NonDaemonProcess => format!(
                "another process holds {path}; retried for {ms}ms and no TopoDB daemon socket \
                 was found, so the holder is a non-daemon process — likely an old pre-daemon \
                 topodb binary, an sgh run (which takes the lock as its own run mutex), or a \
                 language binding that opened the file directly. Stop that process, or run \
                 `topodb daemon start` to serve this DB through a shared daemon \
                 (tune the wait with --lock-wait-ms / TOPODB_LOCK_WAIT_MS)."
            ),
            LikelyHolder::UnusableDaemonSocket => format!(
                "another process holds {path}; retried for {ms}ms. A daemon socket exists but \
                 could not be used (likely protocol/version skew or a failed election), so this \
                 call fell back to a direct open and lost the lock. Check the daemon for this DB \
                 (`topodb daemon status`) rather than hunting a stray process \
                 (tune the wait with --lock-wait-ms / TOPODB_LOCK_WAIT_MS)."
            ),
        }
    }
}

// The socket round-trip (connect → hello → tools/call → unwrap) is implemented
// on unix over `std::os::unix::net::UnixStream` with newline-delimited JSON
// framing — the same framing the daemon uses over stdio. It is NOT yet wired
// into `cli.rs` dispatch (a later step maps each `Command` to its tool call);
// this type is the client half that step will drive.
#[cfg(unix)]
pub use imp::{send_shutdown, DaemonClient};

#[cfg(unix)]
mod imp {
    use super::{hello_frame, unwrap_tool_result, DaemonError, HelloScopes};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    /// Client-side wedge defense: never block forever on a stuck daemon.
    const IO_TIMEOUT: Duration = Duration::from_secs(30);

    /// Ask the daemon at `endpoint` to drain and exit.
    ///
    /// The daemon only recognizes `topodb/shutdown` as the FIRST frame after
    /// the hello (it peeks exactly one line before handing the connection to
    /// rmcp), so this cannot go through [`DaemonClient::connect`], which sends
    /// `initialize` there. Raw sequence instead: hello, then the shutdown
    /// request, then wait for its response so the caller knows the daemon
    /// heard it (the actual exit is confirmed by watching the socket/lock).
    pub fn send_shutdown(endpoint: &Path) -> Result<(), DaemonError> {
        let stream = UnixStream::connect(endpoint)
            .map_err(|e| DaemonError::Transport(format!("connect {}: {e}", endpoint.display())))?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(IO_TIMEOUT)))
            .map_err(|e| DaemonError::Transport(format!("setting socket timeouts: {e}")))?;
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| DaemonError::Transport(format!("cloning socket: {e}")))?,
        );
        let mut writer = stream;
        let scopes = HelloScopes {
            scope: "shared".to_string(),
            read_scopes: vec!["shared".to_string()],
        };
        let shutdown = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "topodb/shutdown",
        });
        writer
            .write_all(format!("{}{shutdown}\n", hello_frame(&scopes)).as_bytes())
            .and_then(|_| writer.flush())
            .map_err(|e| DaemonError::Transport(format!("sending shutdown: {e}")))?;
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| DaemonError::Transport(format!("awaiting shutdown ack: {e}")))?;
        let msg: serde_json::Value = serde_json::from_str(line.trim())
            .map_err(|e| DaemonError::Transport(format!("daemon sent non-JSON ack: {e}")))?;
        match msg.get("error") {
            Some(err) => Err(DaemonError::Protocol(err.to_string())),
            None => Ok(()),
        }
    }

    /// A live, hello'd connection to the daemon, ready to issue tool calls.
    pub struct DaemonClient {
        writer: UnixStream,
        reader: BufReader<UnixStream>,
        next_id: i64,
    }

    impl DaemonClient {
        /// Connect to `endpoint`, send the `topodb/hello` frame, and complete
        /// the MCP `initialize` handshake. A refused connect (stale socket) or
        /// any I/O error is a [`DaemonError::Transport`] — the caller's cue to
        /// fall through to direct open.
        pub fn connect(endpoint: &Path, scopes: &HelloScopes) -> Result<Self, DaemonError> {
            let stream = UnixStream::connect(endpoint).map_err(|e| {
                DaemonError::Transport(format!("connect {}: {e}", endpoint.display()))
            })?;
            stream
                .set_read_timeout(Some(IO_TIMEOUT))
                .and_then(|_| stream.set_write_timeout(Some(IO_TIMEOUT)))
                .map_err(|e| DaemonError::Transport(format!("setting socket timeouts: {e}")))?;
            let reader = BufReader::new(
                stream
                    .try_clone()
                    .map_err(|e| DaemonError::Transport(format!("cloning socket: {e}")))?,
            );
            let mut client = DaemonClient {
                writer: stream,
                reader,
                next_id: 0,
            };

            // Hello FIRST, before any JSON-RPC, so the daemon stamps this
            // connection's scope onto every request — including initialize.
            client.write_line(&hello_frame(scopes))?;

            // Minimal MCP handshake: initialize (request) then initialized
            // (notification). The daemon refuses tool calls before this.
            let _ = client.request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "topodb-cli", "version": env!("CARGO_PKG_VERSION") },
                }),
            )?;
            client.notify("notifications/initialized", serde_json::json!({}))?;
            Ok(client)
        }

        /// Issue a CLI command as its equivalent MCP tool call and unwrap the
        /// result to the same JSON the direct path prints. `tool` is the MCP
        /// tool name; `arguments` its params object (per-command mapping is the
        /// job of the dispatch-wiring step).
        pub fn call_tool(
            &mut self,
            tool: &str,
            arguments: serde_json::Value,
        ) -> Result<serde_json::Value, DaemonError> {
            let result = self.request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
            )?;
            unwrap_tool_result(&result)
        }

        /// Send a JSON-RPC request and return its `result`, mapping a JSON-RPC
        /// `error` to [`DaemonError::Protocol`].
        fn request(
            &mut self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, DaemonError> {
            let id = self.next_id;
            self.next_id += 1;
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });
            self.write_line(&format!("{frame}\n"))?;

            // Read lines until the response with our id arrives, skipping
            // notifications and any other-id traffic on the connection.
            loop {
                let line = self.read_line()?;
                let msg: serde_json::Value = serde_json::from_str(&line).map_err(|e| {
                    DaemonError::Transport(format!("daemon sent non-JSON line: {e}"))
                })?;
                if msg.get("id").and_then(|v| v.as_i64()) != Some(id) {
                    continue;
                }
                if let Some(err) = msg.get("error") {
                    return Err(DaemonError::Protocol(err.to_string()));
                }
                return Ok(msg
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
        }

        /// Send a JSON-RPC notification (no id, no response expected).
        fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), DaemonError> {
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            });
            self.write_line(&format!("{frame}\n"))
        }

        fn write_line(&mut self, line: &str) -> Result<(), DaemonError> {
            self.writer
                .write_all(line.as_bytes())
                .and_then(|_| self.writer.flush())
                .map_err(|e| DaemonError::Transport(format!("writing to daemon: {e}")))
        }

        fn read_line(&mut self) -> Result<String, DaemonError> {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .map_err(|e| DaemonError::Transport(format!("reading from daemon: {e}")))?;
            if n == 0 {
                return Err(DaemonError::Transport(
                    "daemon closed the connection".to_string(),
                ));
            }
            Ok(line)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the process-global `XDG_RUNTIME_DIR` (which
    /// `socket_path_for` reads): under cargo's parallel harness a sibling test
    /// flipping it mid-body would change the derived path out from under this
    /// one. Each holder saves and restores the prior value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(not(windows))]
    fn with_xdg_runtime_dir<T>(dir: &std::path::Path, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", dir);
        let out = body();
        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        out
    }

    // --- endpoint derivation: identical scheme to topodb-mcp's socket_path ---

    #[test]
    fn protocol_tag_tracks_version() {
        assert_eq!(protocol_tag(), PROTOCOL_TAG);
        assert_eq!(PROTOCOL_TAG, format!("v{PROTOCOL_VERSION}"));
    }

    #[test]
    fn hash12_matches_canonical_sha256_vectors() {
        // The cross-crate contract: these are the exact vectors pinned in
        // topodb-mcp's socket_path tests and computed by ipc.js
        // (sha256 -> hex -> slice(0,12)). If this drifts, the CLI and the
        // daemon would resolve a DB path to different endpoints.
        assert_eq!(hash12(""), "e3b0c44298fc");
        assert_eq!(hash12("abc"), "ba7816bf8f01");
    }

    fn endpoint_name(db: &str) -> String {
        socket_path_for(Path::new(db))
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn endpoint_name_embeds_version_tag_and_hash() {
        let name = endpoint_name("/tmp/test.redb");
        let expected_hash = hash12("/tmp/test.redb");
        assert!(name.contains(PROTOCOL_TAG), "missing version tag in {name}");
        assert!(name.contains(&expected_hash), "missing hash in {name}");
        assert!(name.contains(&format!("topodb-{PROTOCOL_TAG}-{expected_hash}")));
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

    #[cfg(not(windows))]
    #[test]
    fn unix_endpoint_is_a_dot_sock() {
        let p = socket_path_for(Path::new("/tmp/test.redb"));
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("sock"));
    }

    // --- socket discovery: present iff the endpoint file exists ---

    #[cfg(not(windows))]
    #[test]
    fn socket_present_at_reflects_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = dir.path().join("topodb-v1-abc123.sock");
        assert!(
            !socket_present_at(&endpoint),
            "a missing endpoint must read as absent"
        );
        std::fs::write(&endpoint, b"").unwrap();
        assert!(
            socket_present_at(&endpoint),
            "an existing endpoint must read as present"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn discover_absent_signals_fall_through() {
        // Point discovery at a runtime dir with no socket in it.
        let dir = tempfile::tempdir().unwrap();
        let got =
            with_xdg_runtime_dir(dir.path(), || discover(Path::new("/tmp/never-served.redb")));
        assert_eq!(got, Discovery::NoSocket);
    }

    #[cfg(not(windows))]
    #[test]
    fn discover_present_returns_the_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        with_xdg_runtime_dir(dir.path(), || {
            let db = Path::new("/tmp/served.redb");
            let endpoint = socket_path_for(db);
            std::fs::write(&endpoint, b"").unwrap();
            match discover(db) {
                Discovery::SocketPresent(p) => assert_eq!(p, endpoint),
                Discovery::NoSocket => panic!("socket exists but discovery reported NoSocket"),
            }
        });
    }

    // --- hello frame: matches ipc.js's helloFrame wire shape ---

    #[test]
    fn hello_frame_shape_matches_ipc_js() {
        let frame = hello_frame(&HelloScopes {
            scope: "01ABC".to_string(),
            read_scopes: vec!["01ABC".to_string(), "shared".to_string()],
        });
        assert!(
            frame.ends_with('\n'),
            "hello frame must be newline-terminated"
        );
        let parsed: serde_json::Value = serde_json::from_str(frame.trim()).unwrap();
        assert_eq!(parsed[HELLO_KEY]["scope"], "01ABC");
        assert_eq!(parsed[HELLO_KEY]["read_scopes"][0], "01ABC");
        assert_eq!(parsed[HELLO_KEY]["read_scopes"][1], "shared");
        // No jsonrpc key — the design keeps hello impossible to confuse with a
        // JSON-RPC message.
        assert!(parsed.get("jsonrpc").is_none());
    }

    // --- Busy-exhaustion classifier: names the likely non-daemon holder ---

    #[test]
    fn busy_without_socket_names_non_daemon_holders() {
        let d = BusyDiagnosis {
            db_path: PathBuf::from("/tmp/memory.redb"),
            lock_wait_ms: 3000,
            socket_present: false,
        };
        assert_eq!(d.likely_holder(), LikelyHolder::NonDaemonProcess);
        let m = d.message();
        assert!(
            m.contains("/tmp/memory.redb"),
            "message names the db path: {m}"
        );
        assert!(
            m.contains("3000ms"),
            "message names the exhausted budget: {m}"
        );
        // The three non-daemon holder classes the design calls out.
        assert!(
            m.contains("old pre-daemon topodb binary"),
            "names old binary: {m}"
        );
        assert!(m.contains("sgh run"), "names sgh: {m}");
        assert!(m.contains("binding"), "names a binding: {m}");
        assert!(m.contains("non-daemon"), "classifies as non-daemon: {m}");
    }

    #[test]
    fn busy_with_socket_points_at_the_daemon_instead() {
        let d = BusyDiagnosis {
            db_path: PathBuf::from("/tmp/memory.redb"),
            lock_wait_ms: 5000,
            socket_present: true,
        };
        assert_eq!(d.likely_holder(), LikelyHolder::UnusableDaemonSocket);
        let m = d.message();
        assert!(
            m.contains("daemon socket exists"),
            "explains the skew case: {m}"
        );
        assert!(
            m.contains("topodb daemon status"),
            "points at the daemon: {m}"
        );
        // It must NOT send the operator hunting a stray process in this case.
        assert!(
            !m.contains("sgh run"),
            "should not enumerate stray holders: {m}"
        );
    }

    // --- tool-result unwrap: same JSON as the direct path ---

    #[test]
    fn unwrap_prefers_structured_content() {
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": "{\"stale\":true}" }],
            "structuredContent": { "id": "01XYZ", "deduplicated": false },
            "isError": false,
        });
        let got = unwrap_tool_result(&result).unwrap();
        assert_eq!(
            got,
            serde_json::json!({ "id": "01XYZ", "deduplicated": false })
        );
    }

    #[test]
    fn unwrap_falls_back_to_text_content_as_json() {
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": "{\"id\":\"01XYZ\",\"created\":true}" }],
            "isError": false,
        });
        let got = unwrap_tool_result(&result).unwrap();
        assert_eq!(got, serde_json::json!({ "id": "01XYZ", "created": true }));
    }

    #[test]
    fn unwrap_is_error_is_a_protocol_error() {
        let result = serde_json::json!({
            "content": [{ "type": "text", "text": "node not found" }],
            "isError": true,
        });
        match unwrap_tool_result(&result) {
            Err(DaemonError::Protocol(m)) => assert!(m.contains("node not found"), "got: {m}"),
            other => panic!("expected a Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn unwrap_rejects_a_shapeless_result() {
        let result = serde_json::json!({ "isError": false });
        assert!(matches!(
            unwrap_tool_result(&result),
            Err(DaemonError::Protocol(_))
        ));
    }
}
