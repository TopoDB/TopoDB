#![cfg(unix)]

//! Regression test: `--socket`/daemon mode must run the SAME boot onboarding
//! (write `CONVENTIONS.md` next to the db + overdue hygiene catch-up) that
//! the stdio path already runs at `main.rs`. The daemon previously only
//! spawned the 300s hygiene tick and never called `run_boot_onboarding`
//! once at startup, so a CC user (whose plugin broker always spawns
//! `--socket`) never got `CONVENTIONS.md` nor boot-time hygiene.
//!
//! This test spawns a real `topodb-mcp --socket` child against a fresh temp
//! db and polls (bounded, no fixed long sleep) for `CONVENTIONS.md` to
//! appear next to the db file.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct DaemonGuard {
    child: Child,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn socket_mode_writes_conventions_md_on_boot() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let db_path = temp_dir.path().join("test.redb");
    let socket_path: PathBuf = temp_dir.path().join("daemon.sock");
    let conventions_path = temp_dir.path().join("CONVENTIONS.md");

    let child = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
        .arg("--socket")
        .arg(socket_path.to_string_lossy().into_owned())
        .arg("--db")
        .arg(db_path.to_string_lossy().into_owned())
        .arg("--embeddings")
        .arg("off")
        .spawn()
        .expect("failed to spawn daemon");

    let _guard = DaemonGuard { child };

    // Bounded poll (not a fixed sleep): wait up to 5s for CONVENTIONS.md to
    // show up next to the db, which boot onboarding should write promptly
    // (before the accept loop even starts serving connections).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while Instant::now() < deadline {
        if conventions_path.is_file() {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    assert!(
        found,
        "CONVENTIONS.md was not written next to the db within 5s of \
         `topodb-mcp --socket` starting up (path: {}); socket/daemon mode \
         must run boot onboarding the same way stdio mode does",
        conventions_path.display()
    );
}
