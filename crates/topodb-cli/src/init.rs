//! `topodb init` — the human + config-only-client onboarding entrypoint.
//!
//! Scaffolds a working install (db + global `.topodb.toml` + `CONVENTIONS.md`),
//! injects the memory-usage pointer into config-only clients' rules files in
//! the current project, runs overdue hygiene, and (best-effort) starts the
//! daemon. Each step's failure is collected rather than aborting the rest —
//! a full `init` exits non-zero if any step failed; `--if-needed` always
//! exits 0.
//!
//! Runs BEFORE the normal db-open path in `main.rs` (like `conventions` and
//! `daemon`): it creates the db itself, so it must not contend with — or
//! silently reuse — the direct-open path's assumptions.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use topodb::Scope;
use topodb_onboarding::FenceOutcome;

use crate::output;

pub struct InitArgs {
    pub db_path: PathBuf,
    pub scope: Scope,
    pub if_needed: bool,
    pub force: bool,
    pub no_daemon: bool,
    pub no_clients: bool,
    pub lock_wait_ms: u64,
    pub pretty: bool,
}

/// Advisory create-exclusive lock: removed on drop. A simple retry loop is
/// sufficient here — `init` is a rare, human/agent-triggered operation, not
/// a hot path, and the tests are single-process.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_lock(lock_path: &Path, wait_ms: u64) -> Result<LockGuard, ()> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(_) => {
                return Ok(LockGuard {
                    path: lock_path.to_path_buf(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    return Err(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // Anything other than "already locked" (e.g. permission denied)
            // isn't something retrying will fix — treat it like a busy lock
            // so the caller reports/defers consistently rather than
            // spinning until the deadline.
            Err(_) => return Err(()),
        }
    }
}

fn step_write_config(config_path: &Path, db_path: &Path, scope_str: &str) -> Result<(), String> {
    let existing = std::fs::read_to_string(config_path).unwrap_or_default();
    let updates = topodb_onboarding::OnboardingUpdates {
        db: Some(db_path.display().to_string()),
        scope: Some(scope_str.to_string()),
        onboarding_version: topodb_onboarding::ONBOARDING_VERSION,
        ensure_schedule_defaults: true,
    };
    let new_text = topodb_onboarding::render_merged(&existing, &updates);
    std::fs::write(config_path, new_text).map_err(|e| e.to_string())
}

/// Small helper trait so a missing-file `read_to_string` reads as "no
/// existing version" rather than a hard error, while any other IO error
/// (permissions, etc.) still propagates as a step failure.
trait MissingIsNone<T> {
    fn ok_or_none_on_missing(self) -> Result<Option<T>, String>;
}

impl MissingIsNone<String> for std::io::Result<String> {
    fn ok_or_none_on_missing(self) -> Result<Option<String>, String> {
        match self {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Upserts the pointer fence into `path`. When `create_if_missing` is
/// false and the file doesn't exist, this is a no-op (config-only clients
/// like `.cursor/rules`/`.windsurfrules`/`.clinerules` are refreshed
/// in-place only — `init` never opts a project into a client it doesn't
/// already use).
fn inject_pointer(path: &Path, block: &str, create_if_missing: bool) -> Result<(), String> {
    let existing = match std::fs::read_to_string(path).ok_or_none_on_missing()? {
        Some(t) => t,
        None if create_if_missing => String::new(),
        None => return Ok(()),
    };
    let (new_text, outcome) =
        topodb_onboarding::upsert_fence(&existing, block, topodb_onboarding::ONBOARDING_VERSION);
    if matches!(outcome, FenceOutcome::Injected | FenceOutcome::Replaced) {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
        }
        std::fs::write(path, &new_text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn step_inject_clients(cwd: &Path) -> Vec<String> {
    let block = topodb_onboarding::pointer_block();
    let mut errors = Vec::new();
    if let Err(e) = inject_pointer(&cwd.join("AGENTS.md"), &block, true) {
        errors.push(format!("AGENTS.md: {e}"));
    }
    for rel in [".cursor/rules", ".windsurfrules", ".clinerules"] {
        if let Err(e) = inject_pointer(&cwd.join(rel), &block, false) {
            errors.push(format!("{rel}: {e}"));
        }
    }
    errors
}

/// Best-effort daemon start via a `topodb --db <path> daemon start`
/// subprocess (rather than reusing `main::daemon_start`, which is a `-> !`
/// terminal handler). Failure here is reported but never aborts `init` —
/// callers that care about a running daemon can always retry
/// `topodb daemon start` themselves.
fn try_start_daemon(db_path: &Path) -> Result<(), String> {
    let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(current_exe)
        .arg("--db")
        .arg(db_path)
        .arg("daemon")
        .arg("start")
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Runs `topodb init`. Never returns — always exits via `output::ok`/`fail`
/// or `std::process::exit`.
pub fn run_init(args: InitArgs) -> ! {
    let config_dir = args
        .db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let config_path = config_dir.join(".topodb.toml");
    let lock_path = config_dir.join(".topodb.init.lock");

    // Fast path: no writes, no lock — just a read of the marker.
    if args.if_needed && !args.force {
        if let Ok(text) = std::fs::read_to_string(&config_path) {
            let cfg = topodb_onboarding::parse(&text);
            if cfg.onboarding_version == Some(topodb_onboarding::ONBOARDING_VERSION) {
                let summary = serde_json::json!({
                    "status": "already-initialized",
                    "onboarding_version": topodb_onboarding::ONBOARDING_VERSION,
                });
                output::ok(&summary, args.pretty);
            }
        }
    }

    if let Err(e) = std::fs::create_dir_all(&config_dir) {
        output::fail(
            "internal",
            &format!("creating {}: {e}", config_dir.display()),
            1,
        );
    }

    let lock = acquire_lock(&lock_path, args.lock_wait_ms);
    let _lock = match lock {
        Ok(g) => g,
        Err(()) => {
            if args.if_needed {
                let summary = serde_json::json!({
                    "status": "deferred",
                    "reason": "init lock busy",
                });
                output::ok(&summary, args.pretty);
            } else {
                output::fail(
                    "busy",
                    "another `topodb init` is in progress (lock busy)",
                    3,
                );
            }
        }
    };

    let mut errors: Vec<String> = Vec::new();
    let mut steps: Vec<serde_json::Value> = Vec::new();

    // Step 1: create/open the db.
    let db = match topodb::Db::open_with(&args.db_path, topodb_json::default_spec()) {
        Ok(db) => {
            steps.push(serde_json::json!({"step": "db", "ok": true}));
            Some(db)
        }
        Err(e) => {
            let msg = format!("opening db {}: {e}", args.db_path.display());
            steps.push(serde_json::json!({"step": "db", "ok": false, "error": msg}));
            errors.push(msg);
            None
        }
    };

    // Step 2: global .topodb.toml (merge, never clobber user values).
    let scope_str = topodb_json::scope_label(&args.scope);
    match step_write_config(&config_path, &args.db_path, &scope_str) {
        Ok(()) => steps.push(serde_json::json!({"step": "config", "ok": true})),
        Err(e) => {
            steps.push(serde_json::json!({"step": "config", "ok": false, "error": e}));
            errors.push(format!("config: {e}"));
        }
    }

    // Step 3: CONVENTIONS.md (write if missing/older).
    match topodb_onboarding::ensure_conventions_file(&config_dir).map_err(|e| e.to_string()) {
        Ok(wrote) => {
            steps.push(serde_json::json!({"step": "conventions", "ok": true, "wrote": wrote}))
        }
        Err(e) => {
            steps.push(serde_json::json!({"step": "conventions", "ok": false, "error": e}));
            errors.push(format!("conventions: {e}"));
        }
    }

    // Step 4: config-only client injection (project cwd).
    if !args.no_clients {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let client_errors = step_inject_clients(&cwd);
        if client_errors.is_empty() {
            steps.push(serde_json::json!({"step": "clients", "ok": true}));
        } else {
            steps
                .push(serde_json::json!({"step": "clients", "ok": false, "errors": client_errors}));
            errors.extend(client_errors);
        }
    }

    // Step 5: overdue hygiene catch-up (bounded tasks only; heavy reingest
    // stays deferred — same contract as `run_catch_up`'s `allow_heavy: false`).
    if let Some(db) = &db {
        let cfg_text = std::fs::read_to_string(&config_path).unwrap_or_default();
        let cfg = topodb_onboarding::parse(&cfg_text);
        let sources = topodb_onboarding::resolve_sources(
            &config_dir,
            topodb_onboarding::env_home().as_deref(),
            &cfg.sources,
        );
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match topodb_onboarding::run_catch_up(
            db,
            args.scope,
            &cfg.schedule,
            &sources,
            now_ms,
            false,
        ) {
            Ok(report) => steps.push(serde_json::json!({
                "step": "hygiene",
                "ok": true,
                "ran": report.ran.len(),
                "deferred": report.deferred.len(),
            })),
            Err(e) => {
                let msg = format!("{e:?}");
                steps.push(serde_json::json!({"step": "hygiene", "ok": false, "error": msg}));
                errors.push(format!("hygiene: {msg}"));
            }
        }
    } else {
        errors.push("hygiene: skipped (db not open)".to_string());
    }

    // Step 6: daemon (best-effort; not test-critical, callers pass
    // --no-daemon). Hygiene already ran in step 5, so a failed/slow daemon
    // start is a bonus, not a fatal `init` step — its failure is recorded
    // in `steps`/`warnings` but never pushed into the fatal `errors` vec.
    let mut warnings: Vec<String> = Vec::new();
    if !args.no_daemon {
        match try_start_daemon(&args.db_path) {
            Ok(()) => steps.push(serde_json::json!({"step": "daemon", "ok": true})),
            Err(e) => {
                steps.push(serde_json::json!({"step": "daemon", "ok": false, "error": e}));
                warnings.push(format!("daemon: {e}"));
            }
        }
    }

    drop(_lock);

    let has_fatal_errors = !errors.is_empty();

    let summary = serde_json::json!({
        "status": if has_fatal_errors { "error" } else { "ok" },
        "onboarding_version": topodb_onboarding::ONBOARDING_VERSION,
        "db": args.db_path.display().to_string(),
        "config": config_path.display().to_string(),
        "steps": steps,
        "errors": errors,
        "warnings": warnings,
    });

    if !has_fatal_errors {
        output::ok(&summary, args.pretty);
    } else {
        let text = if args.pretty {
            serde_json::to_string_pretty(&summary).unwrap_or_else(|_| summary.to_string())
        } else {
            summary.to_string()
        };
        eprintln!("{text}");
        std::process::exit(init_exit_code(args.if_needed, has_fatal_errors));
    }
}

/// Process exit code for `init`. `--if-needed` never hard-fails a client
/// startup, so it always exits 0 regardless of step errors; a full `init`
/// exits non-zero when any fatal step failed.
fn init_exit_code(if_needed: bool, has_fatal_errors: bool) -> i32 {
    if if_needed {
        0
    } else if has_fatal_errors {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::init_exit_code;

    #[test]
    fn if_needed_always_exits_zero() {
        assert_eq!(init_exit_code(true, true), 0);
        assert_eq!(init_exit_code(true, false), 0);
    }

    #[test]
    fn full_init_exits_one_on_fatal_error() {
        assert_eq!(init_exit_code(false, true), 1);
        assert_eq!(init_exit_code(false, false), 0);
    }
}
