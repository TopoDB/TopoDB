#![cfg(windows)]

//! Windows named-pipe routing for the `topodb` CLI.
//!
//! The unix parity/multiprocess suites (`daemon_parity.rs`, `multiprocess.rs`)
//! are `#![cfg(unix)]` — they build on `std::os::unix::net::UnixStream` and
//! never run on Windows. This file is their Windows analog and the ONLY runtime
//! proof of the Windows named-pipe CLI client; it runs on CI `test
//! (windows-latest)`.
//!
//! Two contracts are exercised:
//!  1. **Routing + `status`/`stop` under a resident daemon.** With a daemon
//!     (spawned `--embeddings off`, so it answers promptly) holding the redb
//!     lock, a `topodb <cmd>` must route to it over the named pipe and return
//!     output — NOT fall through to a direct open and lose the lock with `Busy`.
//!     This test also drives `daemon status` and `daemon stop` against that
//!     well-behaved daemon. It closes exactly the asymmetry the Windows client
//!     fixes: before it, a Windows CLI call while a plugin daemon was resident
//!     failed `Busy`.
//!  2. **`daemon start` + `status`.** The start command discovers
//!     `topodb-mcp.exe` beside the CLI and spawns it detached (`creation_flags`);
//!     `status` then confirms it is live over the pipe. This test does NOT call
//!     `daemon stop`: `start` spawns the daemon with default embeddings, and the
//!     Windows client has no read deadline yet (see the test body), so a slow
//!     shutdown ack could hang. `stop` is covered by test 1's well-behaved
//!     daemon instead; here cleanup is by idle-reap.
//!
//! Lesson baked in (see the daemon arc memory): a running `topodb-mcp.exe`
//! holds its `.redb` open, so every daemon a test spawns gets a bounded
//! `TOPODB_DAEMON_IDLE_MS` as a reap backstop and is explicitly stopped, so the
//! tempdir teardown does not race a live holder. The window must NOT be too
//! short: a poll probe opens-and-drops the pipe, so `handle_conn` hits EOF at
//! once (not after the hello timeout) and the daemon's idle countdown starts as
//! soon as `daemon start` returns. A fresh Windows CLI spawn for the *next*
//! command (exe copy + Defender scan) routinely takes a couple of seconds on a
//! cold CI runner — launch.js hit exactly this and widened its own budget — so
//! a 3s idle could reap the daemon between `start` and `status`. 15s clears that
//! gap while still bounding the teardown race on the panic path.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The `topodb-mcp.exe` daemon binary sits beside the `topodb.exe` CLI in the
/// same target dir. `cargo test -p topodb-cli` in isolation may not have built
/// it, so tests skip when it is absent (the workspace test gate builds it
/// first). Note the `.exe` — the CLI's own `daemon start` joins the same name.
fn daemon_bin() -> Option<PathBuf> {
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_topodb"));
    let bin = cli.parent()?.join("topodb-mcp.exe");
    bin.exists().then_some(bin)
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_topodb"))
}

/// Run a `topodb --db <db> <args...>` invocation and return trimmed stdout.
/// `TOPODB_DAEMON_IDLE_MS` is set so any daemon a subcommand spawns (`daemon
/// start`) inherits a reap backstop long enough to survive the next cold Windows
/// CLI spawn (see the module lesson).
fn run(db: &Path, args: &[&str]) -> String {
    let out = cli()
        .env("TOPODB_DAEMON_IDLE_MS", "15000")
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("spawn topodb");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Kills the daemon child on drop so a failed assertion never leaks a lock
/// holder that would block the tempdir teardown's file removal.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the daemon directly and wait until `topodb daemon status` reports it
/// live. The bounded idle reaps it if a panic skips the explicit stop.
fn start_daemon(bin: &Path, db: &Path) -> DaemonGuard {
    let child = Command::new(bin)
        .env("TOPODB_DAEMON_IDLE_MS", "15000")
        .arg("--socket")
        .arg("--db")
        .arg(db)
        .arg("--embeddings")
        .arg("off")
        .spawn()
        .expect("spawn daemon");
    let guard = DaemonGuard(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let status = run(db, &["daemon", "status"]);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&status) {
            if v.get("live").and_then(|b| b.as_bool()) == Some(true) {
                return guard;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon never became live over the named pipe");
}

/// Parse the `live` boolean out of a `daemon status` JSON payload.
fn status_live(db: &Path) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(&run(db, &["daemon", "status"]))
        .ok()
        .and_then(|v| v.get("live").and_then(|b| b.as_bool()))
}

/// Assert `daemon stop` confirmed, so the file is unlocked before teardown.
fn stop_and_confirm(db: &Path) {
    let stop = run(db, &["daemon", "stop"]);
    let stopped = serde_json::from_str::<serde_json::Value>(&stop)
        .ok()
        .and_then(|v| v.get("stopped").and_then(|b| b.as_bool()));
    assert_eq!(stopped, Some(true), "daemon stop did not confirm: {stop}");
}

#[test]
fn routes_over_named_pipe_instead_of_busy() {
    let Some(bin) = daemon_bin() else {
        eprintln!("skipping: target topodb-mcp.exe not built (run the workspace build first)");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("winroute.redb");

    // Fixture (no daemon yet): one memory + entity, and capture the entity id.
    run(
        &db,
        &[
            "remember",
            "--content",
            "delta fact about otters",
            "--entity",
            "Otter",
        ],
    );
    let ent = {
        let out = run(
            &db,
            &[
                "find", "--label", "Entity", "--prop", "name", "--value", "Otter",
            ],
        );
        let v: serde_json::Value = serde_json::from_str(&out).expect("find json");
        v[0]["id"].as_str().expect("entity id").to_string()
    };

    // Serve the SAME file from a daemon (it now holds the redb lock) and route.
    let _daemon = start_daemon(&bin, &db);

    // `search` is the hot read path: routed, it stays a bare array and finds the
    // memory rather than hitting Busy.
    let search = run(&db, &["search", "otters"]);
    // `get` routes to `get_node`, which prints `{"found":true,"node":{..}}`
    // (identical shape to the direct path — the parity contract).
    let get = run(&db, &["get", &ent]);

    // Stop before assertions so a failure cannot leave the lock held.
    stop_and_confirm(&db);

    assert!(
        search.starts_with('['),
        "routed search must be a bare array (available, not Busy): {search}"
    );
    let hits: serde_json::Value = serde_json::from_str(&search).expect("routed search json");
    assert!(
        hits.as_array().is_some_and(|a| !a.is_empty()),
        "routed search should recall the otters memory: {search}"
    );
    // Not a Busy error, and the routed get returns the requested entity nested
    // under `node` — proving the command reached the daemon, not a direct open.
    assert!(
        !get.contains("\"kind\":\"busy\""),
        "routed get must not be a Busy error: {get}"
    );
    let got: serde_json::Value = serde_json::from_str(&get).expect("routed get json");
    assert_eq!(
        got["found"].as_bool(),
        Some(true),
        "routed get should find the entity: {get}"
    );
    assert_eq!(
        got["node"]["id"].as_str(),
        Some(ent.as_str()),
        "routed get should return the requested entity: {get}"
    );
}

#[test]
fn daemon_start_status_stop_lifecycle() {
    if daemon_bin().is_none() {
        eprintln!("skipping: target topodb-mcp.exe not built (run the workspace build first)");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("winlifecycle.redb");

    // Nothing serving yet.
    assert_eq!(
        status_live(&db),
        Some(false),
        "no daemon should be live yet"
    );

    // `daemon start` discovers topodb-mcp.exe beside the CLI and spawns it
    // detached; it returns only once the pipe is live.
    let start = run(&db, &["daemon", "start"]);
    let started = serde_json::from_str::<serde_json::Value>(&start)
        .ok()
        .and_then(|v| v.get("started").and_then(|b| b.as_bool()));
    assert_eq!(started, Some(true), "daemon start did not confirm: {start}");

    // Poll status for live rather than a single shot: a cold Windows CLI spawn
    // can lag, and each status probe also keeps the daemon's idle timer fresh,
    // so a slow first check can't be mistaken for a reaped daemon. If it never
    // reads live, the detached daemon genuinely did not survive `start`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut is_live = status_live(&db);
    while is_live != Some(true) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        is_live = status_live(&db);
    }
    assert_eq!(
        is_live,
        Some(true),
        "daemon status should report live after start"
    );

    // Cleanup is by idle-reap, NOT `daemon stop`, on purpose. `daemon start`
    // spawns the daemon with DEFAULT embeddings (the CLI has no flag to disable
    // them), and the Windows named-pipe client currently has no read deadline
    // (unix has a 30s one), so a `daemon stop` whose shutdown ack is slow to
    // arrive would block forever and wedge the whole test run. Until the client
    // grows a Windows I/O timeout (tracked follow-up), the stop path is exercised
    // by `routes_over_named_pipe_instead_of_busy` against a well-behaved
    // `--embeddings off` daemon; here we only prove `start` + `status` on the
    // detached daemon and let its idle timer reap it. The CI job's
    // `timeout-minutes` is the backstop if any client call ever does hang.
}
