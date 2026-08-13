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
//!  1. **Routing under a resident daemon.** With a daemon holding the redb lock,
//!     a `topodb <cmd>` must route to it over the named pipe and return output —
//!     NOT fall through to a direct open and lose the lock with `Busy`. This is
//!     exactly the asymmetry the Windows client closes: before it, a Windows CLI
//!     call while a plugin daemon was resident failed `Busy`.
//!  2. **`daemon start|status|stop`.** The start command discovers
//!     `topodb-mcp.exe` beside the CLI and spawns it detached
//!     (`creation_flags`), and status/stop drive the same named-pipe probe and
//!     shutdown the client uses.
//!
//! Lesson baked in (see the daemon arc memory): a running `topodb-mcp.exe`
//! holds its `.redb` open, so every daemon a test spawns gets a short
//! `TOPODB_DAEMON_IDLE_MS` as a reap backstop and is explicitly stopped, so the
//! tempdir teardown does not race a live holder.

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
/// start`) inherits a reap backstop.
fn run(db: &Path, args: &[&str]) -> String {
    let out = cli()
        .env("TOPODB_DAEMON_IDLE_MS", "3000")
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
/// live. A short idle reaps it if a panic skips the explicit stop.
fn start_daemon(bin: &Path, db: &Path) -> DaemonGuard {
    let child = Command::new(bin)
        .env("TOPODB_DAEMON_IDLE_MS", "3000")
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
    // `get` routes to `get_node` and returns the entity object.
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
    let node: serde_json::Value = serde_json::from_str(&get).expect("routed get json");
    assert_eq!(
        node["id"].as_str(),
        Some(ent.as_str()),
        "routed get should return the requested entity: {get}"
    );
    assert!(
        !get.contains("\"kind\":\"busy\""),
        "routed get must not be a Busy error: {get}"
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
    let before = run(&db, &["daemon", "status"]);
    let before_live = serde_json::from_str::<serde_json::Value>(&before)
        .ok()
        .and_then(|v| v.get("live").and_then(|b| b.as_bool()));
    assert_eq!(
        before_live,
        Some(false),
        "no daemon should be live yet: {before}"
    );

    // `daemon start` discovers topodb-mcp.exe beside the CLI and spawns it
    // detached; it returns only once the pipe is live.
    let start = run(&db, &["daemon", "start"]);
    let started = serde_json::from_str::<serde_json::Value>(&start)
        .ok()
        .and_then(|v| v.get("started").and_then(|b| b.as_bool()));
    assert_eq!(started, Some(true), "daemon start did not confirm: {start}");

    // Now live over the pipe.
    let live = run(&db, &["daemon", "status"]);
    let is_live = serde_json::from_str::<serde_json::Value>(&live)
        .ok()
        .and_then(|v| v.get("live").and_then(|b| b.as_bool()));
    assert_eq!(
        is_live,
        Some(true),
        "daemon status should report live: {live}"
    );

    // Stop it and confirm the lock is released.
    stop_and_confirm(&db);
    let after = run(&db, &["daemon", "status"]);
    let after_live = serde_json::from_str::<serde_json::Value>(&after)
        .ok()
        .and_then(|v| v.get("live").and_then(|b| b.as_bool()));
    assert_eq!(
        after_live,
        Some(false),
        "daemon should be gone after stop: {after}"
    );
}
