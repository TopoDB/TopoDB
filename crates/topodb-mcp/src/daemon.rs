//! The resident **daemon** serving mode of `topodb-mcp`.
//!
//! One `.redb` file admits exactly one OS process — redb takes an exclusive
//! `flock` at open. Rather than have every other front end (other sessions,
//! Bash-spawned CLI calls, hooks) contend on that lock and fail `Busy`, the
//! lock holder promotes itself to a daemon: it binds a per-DB, per-user unix
//! socket and multiplexes every client onto the one shared [`Db`]. Reads run
//! concurrently under the engine's MVCC snapshots; writes funnel into its
//! existing single-applier group commit. This is the Rust successor to the
//! plugin's `broker.js`, with the broker's id-rewriting and cancellation
//! translation **retired** — each connection is its own rmcp session, so it
//! needs neither.
//!
//! [`serve`] is the entrypoint. It implements the lifecycle the broker proved,
//! ported per `docs/superpowers/specs/2026-08-12-topodb-daemon-design.md`:
//!
//! 1. **Election by redb lock.** Open the DB first; only the lock winner binds
//!    the socket. A loser ([`TopoError::Busy`]) exits without unlinking
//!    anything — its client simply connects to the winner.
//! 2. **Stale-socket recovery.** On bind failure with a present-but-dead socket
//!    (connect refused), unlink and rebind. A live socket, or one we did not
//!    bind, is never unlinked.
//! 3. **Per-connection rmcp session.** Each accepted connection gets its own
//!    task and its own rmcp service over the shared `Db`.
//! 4. **Hello handshake.** A connection must open with a `topodb/hello` frame
//!    stamping its `scope`/`read_scopes` (see [`crate::server::TopoServer::for_hello`]);
//!    a connection that never says hello is refused with JSON-RPC error
//!    `-32002`. A per-connection handshake timeout is the wedge defense: a
//!    stuck client cannot pin the daemon open.
//! 5. **Idle-exit.** After `TOPODB_DAEMON_IDLE_MS` (default 60s, `0` = never)
//!    with zero connections, close the listener FIRST, re-check the count, then
//!    drop the `Db` (releasing the flock) and exit. Listener-first closes the
//!    accept race.
//! 6. **Startup budget.** A startup that cannot open the DB within its budget
//!    surfaces the error to the caller (nonzero exit) rather than lingering.

// Socket serving is implemented on unix (unix-domain sockets) and Windows
// (named pipes); on any OTHER platform `serve` opens the DB then returns
// `Unsupported`, so the accept-loop machinery is legitimately dead there.
// Suppress that noise only on those exotic targets, so unix AND Windows both
// get real dead-code lints.
#![cfg_attr(not(any(unix, windows)), allow(dead_code, unused_imports))]

use std::path::PathBuf;
// `Path` is only referenced by the unix `bind_or_reclaim` signature.
#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

use topodb::{Db, TopoError};

use crate::config::Config;
use crate::embedder::Embedder;
use crate::server::TopoServer;
use crate::socket_path::socket_path_for;

// The connection-serving machinery (accept loop, per-connection rmcp session,
// hello handshake, idle/shutdown lifecycle) is shared by both transports.
#[cfg(any(unix, windows))]
use rmcp::ServiceExt;
#[cfg(any(unix, windows))]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(any(unix, windows))]
use std::sync::Arc;
#[cfg(any(unix, windows))]
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(any(unix, windows))]
use tokio::sync::Notify;

// Transport-specific listener (the accepted stream's type is inferred and
// handed straight to the generic `handle_conn`).
#[cfg(unix)]
use tokio::net::UnixListener;

/// The first-frame key a client sends to announce its scopes. MUST match
/// `HELLO_KEY` in `plugins/claude-code/ipc.js` and the broker.
const HELLO_KEY: &str = "topodb/hello";

/// JSON-RPC error code for a connection that issued a request before ever
/// sending a `topodb/hello`. Matches `broker.js`'s refusal so both servers
/// speak the same dialect to an out-of-date client.
#[cfg(any(unix, windows))]
const HELLO_REFUSED_CODE: i32 = -32002;

/// Human-readable body of the `-32002` refusal.
#[cfg(any(unix, windows))]
const HELLO_REFUSED_MSG: &str = "topodb daemon: this connection never sent a scope hello; \
     refusing to run the request under another project's scope \
     (restart the session; if it persists, the client and daemon are out of step)";

/// Default idle window before a connectionless daemon exits, in ms.
const DEFAULT_IDLE_MS: u64 = 60_000;

/// Default per-connection handshake budget, in ms — the wedge defense. A
/// connection that has not sent its hello within this window is dropped.
const DEFAULT_HELLO_MS: u64 = 5_000;

/// How `serve` finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeOutcome {
    /// We won the election, served connections, and exited on idle (or the
    /// listener was closed). The flock has been released.
    Served,
    /// We lost the election — another process already holds the DB lock (very
    /// likely a daemon that won a spawn race). Nothing was bound or unlinked;
    /// the caller's client should connect to the existing socket. Per the
    /// design, a spawner that loses is expected, not an error.
    LostElection,
}

/// Errors that abort daemon startup or serving. All map to a nonzero exit.
#[derive(Debug)]
pub enum DaemonError {
    /// The DB could not be opened for a reason other than the lock being held
    /// (corruption, permissions, an unparseable spec, …). Distinct from
    /// [`ServeOutcome::LostElection`], which is `Busy` and benign.
    Open(TopoError),
    /// Binding (or reclaiming) the socket failed.
    Bind(std::io::Error),
    /// `accept()` failed on the bound listener.
    Accept(std::io::Error),
    /// Socket serving isn't available on this platform (neither unix sockets
    /// nor Windows named pipes). Constructed only in the
    /// `#[cfg(not(any(unix, windows)))]` `run_accept_loop`, hence dead on both
    /// unix and Windows.
    #[cfg_attr(any(unix, windows), allow(dead_code))]
    Unsupported,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Open(e) => write!(f, "daemon could not open the database: {e}"),
            DaemonError::Bind(e) => write!(f, "daemon could not bind its socket: {e}"),
            DaemonError::Accept(e) => write!(f, "daemon accept() failed: {e}"),
            DaemonError::Unsupported => {
                write!(f, "daemon socket serving is not supported on this platform")
            }
        }
    }
}

impl std::error::Error for DaemonError {}

/// Serve TopoDB as a resident daemon over its per-DB unix socket.
///
/// Opens the DB first (the election): the lock winner binds the socket and
/// serves until it idles out; a loser returns [`ServeOutcome::LostElection`]
/// without touching the socket. Any non-`Busy` open failure is
/// [`DaemonError::Open`] (nonzero exit — behavior 6).
///
/// `main.rs` is intentionally left on the stdio path; this entrypoint is wired
/// by the `--socket` CLI seam in a later step.
pub async fn serve(config: &Config) -> Result<ServeOutcome, DaemonError> {
    // (1) Election. Opening the redb DB takes its exclusive flock; the winner
    // is the daemon. `Busy` means another process already holds the lock — we
    // lost, so return WITHOUT binding or unlinking anything.
    let db = match open_db(config) {
        Ok(db) => db,
        Err(TopoError::Busy) => return Ok(ServeOutcome::LostElection),
        Err(e) => return Err(DaemonError::Open(e)),
    };

    let embedder = build_embedder(config);
    let base = TopoServer::new(db.clone(), config, embedder);
    // Explicit socket path from --socket PATH takes precedence; otherwise derive from DB path.
    let endpoint = match &config.socket {
        Some(Some(path)) => path.clone(),
        _ => socket_path_for(&resolved_db_path(config)),
    };

    let idle_ms = parse_idle_ms(std::env::var("TOPODB_DAEMON_IDLE_MS").ok().as_deref());
    let hello_ms = parse_hello_ms(std::env::var("TOPODB_DAEMON_HELLO_MS").ok().as_deref());

    // Best-effort install-time onboarding: ensure CONVENTIONS.md sits next to
    // the db and catch up on overdue hygiene, ONCE at daemon startup — the
    // same call the stdio path makes in `main.rs`. Socket/daemon mode is the
    // path the Claude Code plugin's broker always spawns, so without this
    // call CC users never got CONVENTIONS.md nor boot-time hygiene: the
    // periodic tick below is a bonus on top of a resident daemon, not a
    // substitute for the once-at-startup run. Never allowed to fail or slow
    // boot (see `onboard_boot`'s module doc).
    let onboard_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    crate::onboard_boot::run_boot_onboarding(
        &db,
        &config.db_path,
        config.default_scope,
        onboard_now_ms,
    );

    spawn_hygiene_tick(&db, config);

    run_accept_loop(base, db, endpoint, idle_ms, Duration::from_millis(hello_ms)).await
}

/// Spawns the daemon's low-frequency hygiene tick: the SAME catch-up boot
/// runs, but with `allow_heavy = true` so the heavy re-ingest work boot
/// defers gets a chance to run while the daemon happens to be resident.
/// Genuinely best-effort — the daemon idle-exits in ~60s by default, so this
/// rarely fires; each task's `META` `last_run` gate makes a tick a no-op
/// when nothing is due, so it never duplicates the CLI/server catch-up.
///
/// Deliberately a bare `tokio::spawn` with its own interval, independent of
/// the accept loop's idle-gate/shutdown machinery: it must never count as
/// "activity" that keeps a connectionless daemon alive, and it must never be
/// able to crash the daemon (every catch-up error is swallowed inside
/// [`crate::onboard_boot::tick_once`]).
fn spawn_hygiene_tick(db: &Db, config: &Config) {
    let tick_db = db.clone();
    let tick_scope = config.default_scope;
    let tick_schedule = crate::onboard_boot::resolve_schedule(&config.db_path);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(300));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            iv.tick().await;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            crate::onboard_boot::tick_once(&tick_db, tick_scope, &tick_schedule, now_ms);
        }
    });
}

/// Open the DB with the same spec/upgrade/busy-retry policy `main.rs` uses for
/// the stdio path — so a file either front end already populated is inherited
/// verbatim, and a fresh file gets the canonical default spec. The busy budget
/// comes from `TOPODB_LOCK_WAIT_MS` (warn+default on garbage, aligning with the
/// design's ride-along note). A `Busy` return here is the lost-election signal.
fn open_db(config: &Config) -> Result<Db, TopoError> {
    // Shared parse/default policy with the CLI and the stdio server
    // (`topodb_json::lock_wait_budget_ms`).
    let (budget_ms, lock_wait_warn) =
        topodb_json::lock_wait_budget_ms(std::env::var("TOPODB_LOCK_WAIT_MS").ok().as_deref());
    if let Some(warn) = lock_wait_warn {
        eprintln!("topodb daemon: {warn}");
    }
    topodb_json::open_with_busy_retry(budget_ms, || match &config.spec {
        Some(spec) => Db::open_with(&config.db_path, spec.clone()),
        None if config.db_path.exists() => {
            let db = Db::open_stored(&config.db_path)?;
            let persisted = db.index_spec();
            let upgraded = topodb_json::upgraded_spec(persisted.clone());
            if upgraded != persisted {
                drop(db);
                Db::open_with(&config.db_path, upgraded)
            } else {
                Ok(db)
            }
        }
        None => Db::open_with(&config.db_path, topodb_json::default_spec()),
    })
}

/// Build the embedder handle exactly as `main.rs` does: `off` disables it,
/// `auto`/omitted starts the default model, anything else names a model.
fn build_embedder(config: &Config) -> Embedder {
    let cache_dir = || {
        config
            .model_dir
            .clone()
            .unwrap_or_else(default_model_cache_dir)
    };
    match config.embeddings.as_deref() {
        Some(s) if s.eq_ignore_ascii_case("off") => Embedder::disabled(),
        Some(s) if s.eq_ignore_ascii_case("auto") => {
            Embedder::start(None, cache_dir(), !config.no_ort_download)
        }
        other => Embedder::start(
            other.map(str::to_string),
            cache_dir(),
            !config.no_ort_download,
        ),
    }
}

/// `$HOME/.cache/topodb/models`, or `.topodb-models` when `$HOME` is unset —
/// the same fallback `main.rs` uses for `--model-dir`.
fn default_model_cache_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home)
            .join(".cache")
            .join("topodb")
            .join("models"),
        _ => PathBuf::from(".topodb-models"),
    }
}

/// Absolutize `config.db_path` for socket derivation without resolving
/// symlinks (matching JS `path.resolve`, which `socket_path_for` documents as
/// its input contract). Lexical normalization of `..`/`.` is the launcher's
/// responsibility — it passes an already-resolved path.
fn resolved_db_path(config: &Config) -> PathBuf {
    let p = &config.db_path;
    if p.is_absolute() {
        p.clone()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.clone())
    }
}

/// The idle window in ms: absent → default, `0` → never, garbage → warn+default.
fn parse_idle_ms(raw: Option<&str>) -> u64 {
    match raw {
        None => DEFAULT_IDLE_MS,
        Some(s) => s.parse::<u64>().unwrap_or_else(|_| {
            eprintln!(
                "topodb daemon: ignoring unparseable TOPODB_DAEMON_IDLE_MS={s:?}; \
                 using {DEFAULT_IDLE_MS}"
            );
            DEFAULT_IDLE_MS
        }),
    }
}

/// The handshake window in ms. The timeout is a wedge defense and cannot be
/// disabled: `0` or garbage falls back to the default.
fn parse_hello_ms(raw: Option<&str>) -> u64 {
    match raw.and_then(|s| s.parse::<u64>().ok()) {
        Some(ms) if ms > 0 => ms,
        _ => DEFAULT_HELLO_MS,
    }
}

/// A parsed `topodb/hello` frame: the connection's write `scope` and optional
/// `read_scopes` set.
#[derive(Debug)]
struct Hello {
    scope: String,
    read_scopes: Option<Vec<String>>,
}

/// Parse a first-line frame as a `topodb/hello`. Returns `None` for anything
/// that is not a hello (an ordinary JSON-RPC request, or garbage) — the caller
/// treats `None` as "never said hello". A hello with no `read_scopes` yields
/// `None` there, so `for_hello` falls back to the single write scope.
fn parse_hello(line: &str) -> Option<Hello> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let h = v.get(HELLO_KEY)?;
    let scope = h.get("scope")?.as_str()?.to_string();
    let read_scopes = h.get("read_scopes").and_then(|r| r.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect::<Vec<String>>()
    });
    Some(Hello { scope, read_scopes })
}

/// Bind the daemon's listener, reclaiming a socket left by a crashed daemon.
///
/// On `AddrInUse` we probe for a live listener: a successful connect means a
/// daemon we did NOT bind owns it → error out untouched (never unlink a socket
/// we did not bind). A refused connect means the inode is a corpse (Rust never
/// auto-unlinks) → unlink and rebind. We only reach here as the redb-lock
/// winner, so a live socket at our endpoint is anomalous and left alone.
#[cfg(unix)]
fn bind_or_reclaim(endpoint: &Path) -> Result<UnixListener, DaemonError> {
    use std::io::ErrorKind;

    crate::socket_path::ensure_socket_dir(endpoint).map_err(DaemonError::Bind)?;

    match UnixListener::bind(endpoint) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            match std::os::unix::net::UnixStream::connect(endpoint) {
                // Someone is listening — not us. Do not unlink it.
                Ok(_) => Err(DaemonError::Bind(std::io::Error::new(
                    ErrorKind::AddrInUse,
                    "socket endpoint is already served by a live daemon",
                ))),
                // Dead socket: reclaim it.
                Err(ce) if ce.kind() == ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(endpoint).map_err(DaemonError::Bind)?;
                    UnixListener::bind(endpoint).map_err(DaemonError::Bind)
                }
                Err(ce) => Err(DaemonError::Bind(ce)),
            }
        }
        Err(e) => Err(DaemonError::Bind(e)),
    }
}

/// The accept loop: bind, then serve each connection on its own task over the
/// shared `Db`, idling out when connectionless. Implements behaviors 2–6.
#[cfg(unix)]
async fn run_accept_loop(
    base: TopoServer,
    db: Db,
    endpoint: PathBuf,
    idle_ms: u64,
    hello_timeout: Duration,
) -> Result<ServeOutcome, DaemonError> {
    // (2) Bind, reclaiming a dead socket a crashed predecessor may have left.
    let mut listener = bind_or_reclaim(&endpoint)?;

    let active = Arc::new(AtomicUsize::new(0));
    // Woken when the last connection closes, so the idle countdown restarts
    // from that disconnect rather than from the previous accept.
    let idle_gate = Arc::new(Notify::new());
    // Set true by handle_conn when a topodb/shutdown request is received. This
    // is a DURABLE flag, not just the `Notify` below: the shutdown connection's
    // own close also fires `idle_gate` (it was the last connection), and with a
    // `biased` select the idle_gate branch can win and consume the paired
    // shutdown wake before its branch runs. A flag checked at the top of every
    // iteration cannot be lost that way; the `Notify` only ensures the loop
    // wakes promptly to see it.
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_signal = Arc::new(Notify::new());
    // `.max(1)` keeps the timer from being zero-duration when idle is disabled;
    // the branch guard below is what actually disables idle-exit for `0`.
    let idle = Duration::from_millis(idle_ms.max(1));

    loop {
        // Durable shutdown check FIRST: a topodb/shutdown may have arrived while
        // we were servicing another branch. Stop accepting, let in-flight
        // connections drain, then remove our socket, drop the DB (releasing the
        // flock), and exit.
        if shutdown_flag.load(Ordering::SeqCst) {
            drop(listener);
            while active.load(Ordering::SeqCst) != 0 {
                idle_gate.notified().await;
            }
            let _ = std::fs::remove_file(&endpoint);
            drop(base);
            drop(db);
            return Ok(ServeOutcome::Served);
        }

        let idle_sleep = tokio::time::sleep(idle);
        tokio::pin!(idle_sleep);

        tokio::select! {
            // Prefer accepting/servicing over idling: never exit while work is
            // ready to be taken.
            biased;

            accepted = listener.accept() => {
                let (stream, _addr) = accepted.map_err(DaemonError::Accept)?;
                active.fetch_add(1, Ordering::SeqCst);
                let base = base.clone();
                let active = active.clone();
                let gate = idle_gate.clone();
                let flag = shutdown_flag.clone();
                let shutdown = shutdown_signal.clone();
                // (3) Each connection is its own rmcp session on its own task,
                // sharing the one `Db`.
                tokio::spawn(async move {
                    handle_conn(base, stream, hello_timeout, flag, shutdown).await;
                    if active.fetch_sub(1, Ordering::SeqCst) == 1 {
                        // We were the last connection; wake the loop to re-arm
                        // the idle timer from now (or to see the shutdown flag).
                        gate.notify_one();
                    }
                });
            }

            _ = idle_gate.notified() => {
                // A connection closed; loop to re-arm a fresh idle timer (the
                // top-of-loop check catches a shutdown that closed it).
            }

            _ = shutdown_signal.notified() => {
                // topodb/shutdown arrived; loop so the top-of-loop check tears
                // the daemon down.
            }

            _ = &mut idle_sleep, if idle_ms != 0 && active.load(Ordering::SeqCst) == 0 => {
                // (5) Idle with zero connections. Close the listener FIRST so no
                // new connection can be accepted, THEN re-check the count — this
                // ordering closes the accept race.
                drop(listener);
                if active.load(Ordering::SeqCst) == 0 {
                    // Truly idle. We bound this socket, so it is ours to remove;
                    // dropping `base`/`db` then releases the flock and we exit.
                    let _ = std::fs::remove_file(&endpoint);
                    drop(base);
                    drop(db);
                    return Ok(ServeOutcome::Served);
                }
                // A connection slipped in between the count check and the close.
                // We still hold the lock, so rebind and keep serving it.
                listener = bind_or_reclaim(&endpoint)?;
            }
        }
    }
}

/// Serve one connection: read its hello (behavior 4), build a scoped handler,
/// and hand the remainder of the stream to a fresh rmcp session. Detects and
/// handles topodb/shutdown requests by draining and exiting.
#[cfg(any(unix, windows))]
async fn handle_conn<S>(
    base: TopoServer,
    stream: S,
    hello_timeout: Duration,
    shutdown_flag: Arc<AtomicBool>,
    shutdown_signal: Arc<Notify>,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    // `tokio::io::split` works for any AsyncRead+AsyncWrite — a `UnixStream` on
    // unix, a `NamedPipeServer` on Windows — so this handler is transport-blind.
    let (read_half, write_half) = tokio::io::split(stream);
    // The BufReader retains any bytes it read past the hello's newline (a
    // pipelined `initialize`), so nothing is lost when we hand it to rmcp.
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    // (4) Wedge defense: the hello must arrive within the handshake budget, or
    // the connection is dropped so a stuck client cannot pin the daemon open.
    let n = match tokio::time::timeout(hello_timeout, reader.read_line(&mut line)).await {
        Ok(Ok(n)) => n,
        // Timeout, IO error, or clean EOF race: just close.
        _ => return,
    };
    if n == 0 {
        return; // clean EOF before any frame
    }

    let scoped = match parse_hello(&line) {
        Some(h) => match base.for_hello(&h.scope, h.read_scopes.as_deref()) {
            Ok(scoped) => scoped,
            Err(msg) => {
                eprintln!("topodb daemon: refusing connection with bad hello scope: {msg}");
                return;
            }
        },
        None => {
            // Never said hello: refuse with -32002 rather than run under the
            // daemon's own start-up scope (the cross-project-write bug the
            // hello exists to close). Echo the request id if the frame had one.
            let mut write_half = write_half;
            let id = serde_json::from_str::<serde_json::Value>(line.trim())
                .ok()
                .and_then(|v| v.get("id").cloned())
                .unwrap_or(serde_json::Value::Null);
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": HELLO_REFUSED_CODE, "message": HELLO_REFUSED_MSG },
            });
            let mut payload = resp.to_string();
            payload.push('\n');
            let _ = write_half.write_all(payload.as_bytes()).await;
            let _ = write_half.flush().await;
            return;
        }
    };

    // Peek at the next line to detect topodb/shutdown early. If it's shutdown,
    // respond and signal the main loop to drain and exit. Otherwise, hand off
    // to rmcp with the peeked line buffered (BufReader retains it internally).
    let mut next_line = String::new();
    let peek_ok = matches!(
        tokio::time::timeout(hello_timeout, reader.read_line(&mut next_line)).await,
        Ok(Ok(n)) if n > 0
    );

    if peek_ok {
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(next_line.trim()) {
            if msg.get("method").and_then(|m| m.as_str()) == Some("topodb/shutdown") {
                // Respond with success and signal shutdown.
                let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": serde_json::json!({}),
                });
                let mut payload = resp.to_string();
                payload.push('\n');
                let mut write_half = write_half;
                let _ = write_half.write_all(payload.as_bytes()).await;
                let _ = write_half.flush().await;
                // Set the durable flag BEFORE waking the loop: the wake may race
                // with this connection's own close (which also wakes the loop),
                // but the flag the loop checks at the top cannot be lost.
                shutdown_flag.store(true, Ordering::SeqCst);
                shutdown_signal.notify_one();
                return;
            }
        }
    }

    // Hand the rest of the connection to a fresh rmcp session over the shared
    // `Db`. Reads run concurrently under MVCC; writes serialize on the engine's
    // single applier — no id-rewriting or cancellation-translation.
    //
    // The shutdown peek above CONSUMED bytes from the reader (`read_line`
    // moves the line out of the BufReader; on a timeout, a partial line may
    // have been moved too). Chain them back in front of the stream, or the
    // client's first request after hello — its `initialize` — would silently
    // vanish and the session would hang.
    let replay = std::io::Cursor::new(next_line.into_bytes());
    match scoped
        .serve((tokio::io::AsyncReadExt::chain(replay, reader), write_half))
        .await
    {
        Ok(service) => {
            let _ = service.waiting().await;
        }
        Err(e) => {
            eprintln!("topodb daemon: session ended during initialize: {e}");
        }
    }
}

/// Windows accept loop over a named pipe. Mirrors the unix loop's lifecycle
/// (per-connection rmcp session on the shared `Db`, idle-exit, durable shutdown
/// flag, drain), but the transport is a `NamedPipeServer` instance rather than a
/// `UnixListener`. Two Windows conveniences over unix: named pipes VANISH when
/// the owning process exits (no stale inode to reclaim, nothing to unlink on
/// exit), and `first_pipe_instance(true)` is a belt-and-suspenders bind guard on
/// top of the redb-lock election that already elected us. To accept a client
/// while still serving the previous one, a fresh instance is created up front.
#[cfg(windows)]
async fn run_accept_loop(
    base: TopoServer,
    db: Db,
    endpoint: PathBuf,
    idle_ms: u64,
    hello_timeout: Duration,
) -> Result<ServeOutcome, DaemonError> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let pipe_name = endpoint.into_os_string();
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)
        .map_err(DaemonError::Bind)?;

    let active = Arc::new(AtomicUsize::new(0));
    let idle_gate = Arc::new(Notify::new());
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_signal = Arc::new(Notify::new());
    let idle = Duration::from_millis(idle_ms.max(1));

    loop {
        // Durable shutdown check FIRST (same rationale as the unix loop).
        if shutdown_flag.load(Ordering::SeqCst) {
            drop(server);
            while active.load(Ordering::SeqCst) != 0 {
                idle_gate.notified().await;
            }
            // Named pipes are reclaimed by the OS on process exit — nothing to
            // unlink. Dropping `db` releases the redb lock.
            drop(base);
            drop(db);
            return Ok(ServeOutcome::Served);
        }

        let idle_sleep = tokio::time::sleep(idle);
        tokio::pin!(idle_sleep);

        tokio::select! {
            biased;

            res = server.connect() => {
                res.map_err(DaemonError::Accept)?;
                // Hand off THIS connected instance; stand up the next one BEFORE
                // spawning, so a client arriving mid-handoff is never refused.
                let next = ServerOptions::new()
                    .create(&pipe_name)
                    .map_err(DaemonError::Bind)?;
                let connected = std::mem::replace(&mut server, next);
                active.fetch_add(1, Ordering::SeqCst);
                let base = base.clone();
                let active = active.clone();
                let gate = idle_gate.clone();
                let flag = shutdown_flag.clone();
                let shutdown = shutdown_signal.clone();
                tokio::spawn(async move {
                    handle_conn(base, connected, hello_timeout, flag, shutdown).await;
                    if active.fetch_sub(1, Ordering::SeqCst) == 1 {
                        gate.notify_one();
                    }
                });
            }

            _ = idle_gate.notified() => {}
            _ = shutdown_signal.notified() => {}

            _ = &mut idle_sleep, if idle_ms != 0 && active.load(Ordering::SeqCst) == 0 => {
                // Idle: drop the pending instance to stop accepting, re-check.
                drop(server);
                if active.load(Ordering::SeqCst) == 0 {
                    drop(base);
                    drop(db);
                    return Ok(ServeOutcome::Served);
                }
                // A connection slipped in; stand up a fresh instance and continue.
                server = ServerOptions::new()
                    .create(&pipe_name)
                    .map_err(DaemonError::Bind)?;
            }
        }
    }
}

/// Fallback for platforms that are neither unix nor Windows: no socket transport.
#[cfg(not(any(unix, windows)))]
async fn run_accept_loop(
    _base: TopoServer,
    _db: Db,
    _endpoint: PathBuf,
    _idle_ms: u64,
    _hello_timeout: Duration,
) -> Result<ServeOutcome, DaemonError> {
    Err(DaemonError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_hello() {
        let line =
            r#"{"topodb/hello":{"scope":"6DNBXHJSPZ","read_scopes":["6DNBXHJSPZ","shared"]}}"#;
        let h = parse_hello(line).expect("valid hello");
        assert_eq!(h.scope, "6DNBXHJSPZ");
        assert_eq!(
            h.read_scopes.as_deref(),
            Some(&["6DNBXHJSPZ".to_string(), "shared".to_string()][..])
        );
    }

    #[test]
    fn hello_without_read_scopes_leaves_them_none() {
        let h = parse_hello(r#"{"topodb/hello":{"scope":"shared"}}"#).expect("valid hello");
        assert_eq!(h.scope, "shared");
        assert!(h.read_scopes.is_none());
    }

    #[test]
    fn a_jsonrpc_request_is_not_a_hello() {
        assert!(parse_hello(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).is_none());
    }

    #[test]
    fn garbage_is_not_a_hello() {
        assert!(parse_hello("not json at all").is_none());
        assert!(parse_hello(r#"{"topodb/hello":{}}"#).is_none()); // no scope
    }

    #[test]
    fn idle_ms_defaults_when_absent() {
        assert_eq!(parse_idle_ms(None), DEFAULT_IDLE_MS);
    }

    #[test]
    fn idle_ms_zero_means_never() {
        assert_eq!(parse_idle_ms(Some("0")), 0);
    }

    #[test]
    fn idle_ms_parses_a_value() {
        assert_eq!(parse_idle_ms(Some("1500")), 1500);
    }

    #[test]
    fn idle_ms_falls_back_on_garbage() {
        assert_eq!(parse_idle_ms(Some("later")), DEFAULT_IDLE_MS);
    }

    #[test]
    fn hello_ms_defaults_and_cannot_be_disabled() {
        assert_eq!(parse_hello_ms(None), DEFAULT_HELLO_MS);
        assert_eq!(parse_hello_ms(Some("0")), DEFAULT_HELLO_MS); // wedge defense stays on
        assert_eq!(parse_hello_ms(Some("junk")), DEFAULT_HELLO_MS);
        assert_eq!(parse_hello_ms(Some("250")), 250);
    }

    #[cfg(unix)]
    mod socket {
        use super::*;
        use tempfile::tempdir;

        #[tokio::test]
        async fn binds_a_fresh_endpoint() {
            let dir = tempdir().unwrap();
            let ep = dir.path().join("d.sock");
            let listener = bind_or_reclaim(&ep).expect("fresh bind");
            assert!(ep.exists());
            drop(listener);
        }

        #[tokio::test]
        async fn reclaims_a_dead_socket_file() {
            let dir = tempdir().unwrap();
            let ep = dir.path().join("d.sock");
            // A crashed daemon leaves the socket inode behind — Rust never
            // auto-unlinks — but nothing is listening.
            let first = bind_or_reclaim(&ep).expect("first bind");
            drop(first);
            assert!(ep.exists(), "socket file should survive listener drop");
            // Reclaim path: connect is refused, so we unlink and rebind.
            let second = bind_or_reclaim(&ep).expect("reclaim dead socket");
            drop(second);
        }

        #[tokio::test]
        async fn will_not_unlink_a_live_socket() {
            let dir = tempdir().unwrap();
            let ep = dir.path().join("d.sock");
            let live = bind_or_reclaim(&ep).expect("bind live listener");
            // A second binder connects successfully (the OS backlogs it even
            // without accept()), so it must error rather than steal the socket.
            let err = bind_or_reclaim(&ep).expect_err("must not reclaim a live socket");
            assert!(matches!(err, DaemonError::Bind(_)), "got {err:?}");
            assert!(ep.exists(), "live socket must be left untouched");
            drop(live);
        }
    }
}
