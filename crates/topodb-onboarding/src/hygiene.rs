//! Hygiene catch-up: computes which bounded maintenance tasks are due per
//! the `[schedule]` config, and runs them.
//!
//! `due_tasks` is pure (injectable `last_run` lookup + clock) so it's
//! trivially unit-testable without a `Db`. `run_catch_up` is the impure
//! driver: it reads `last_run` from `Db::get_meta`, runs whichever bounded
//! tasks are due, and records the new `last_run` via `Db::set_meta`.
//!
//! Single-flight (only one process runs catch-up at a time) is the
//! caller's concern (see Task 7/9's lock) — this module assumes it already
//! holds the right to run.

use topodb::{Db, Scope};
use topodb_json::{ComposeError, LifecycleParams};

use crate::config::Schedule;

/// One bounded (or heavy) maintenance task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Task {
    Compact,
    Purge,
    Reingest,
    Lifecycle,
}

impl Task {
    /// The `META` key under which this task's last-run timestamp (ASCII
    /// decimal `i64` ms) is stored.
    pub fn meta_key(&self) -> &'static str {
        match self {
            Task::Compact => "onboarding:last_run:compact",
            Task::Purge => "onboarding:last_run:purge",
            Task::Reingest => "onboarding:last_run:reingest",
            Task::Lifecycle => "onboarding:last_run:lifecycle",
        }
    }

    fn all() -> [Task; 4] {
        [Task::Compact, Task::Purge, Task::Reingest, Task::Lifecycle]
    }

    fn schedule_entry(&self, schedule: &Schedule) -> crate::config::ScheduleEntry {
        match self {
            Task::Compact => schedule.compact,
            Task::Purge => schedule.purge,
            Task::Reingest => schedule.reingest,
            Task::Lifecycle => schedule.lifecycle,
        }
    }
}

/// The outcome of one `run_catch_up` call.
#[derive(Debug, Clone, Default)]
pub struct CatchUpReport {
    /// Tasks that were due and actually ran (including the `Reingest` stub
    /// when `allow_heavy` was set).
    pub ran: Vec<Task>,
    /// Tasks that were due but deferred (currently only `Reingest` when
    /// `!allow_heavy`) — their `last_run` was NOT advanced, so they stay
    /// due until a caller runs catch-up with `allow_heavy: true`.
    pub deferred: Vec<Task>,
    /// Count of lifecycle decay candidates surfaced by the most recent
    /// `Lifecycle` run this call (0 if `Lifecycle` wasn't due/run).
    pub surfaced_candidates: usize,
}

/// Retained-floor for `Compact`: never compact away the most recent
/// `RETAIN` ops, even when the schedule says compaction is due. Keeps a
/// safety margin for `ops_since`-based tail replay (see `Db::subscribe`'s
/// anchoring recipe) without requiring callers to reason about exact
/// seqs.
const RETAIN: u64 = 1000;

/// Purge grace period: a tombstoned memory is only eligible for
/// destructive removal 30 days after it was tombstoned.
const PURGE_GRACE_MS: i64 = 30 * 86_400 * 1000;

/// Pure due-computation: a task is due if its schedule entry is enabled
/// AND (it has never run, or more than `interval_secs * 1000` ms have
/// elapsed since `last_run`). `last_run` is an injectable lookup so this
/// is testable without a `Db`.
pub fn due_tasks(
    schedule: &Schedule,
    last_run: &dyn Fn(Task) -> Option<i64>,
    now_ms: i64,
) -> Vec<Task> {
    Task::all()
        .into_iter()
        .filter(|task| {
            let entry = task.schedule_entry(schedule);
            if !entry.enabled {
                return false;
            }
            match last_run(*task) {
                None => true,
                Some(last) => now_ms - last > (entry.interval_secs as i64) * 1000,
            }
        })
        .collect()
}

fn get_last_run(db: &Db, task: Task) -> Result<Option<i64>, ComposeError> {
    let raw = db.get_meta(task.meta_key()).map_err(ComposeError::Engine)?;
    Ok(raw.and_then(|bytes| {
        std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
    }))
}

fn set_last_run(db: &Db, task: Task, now_ms: i64) -> Result<(), ComposeError> {
    db.set_meta(task.meta_key(), now_ms.to_string().as_bytes())
        .map_err(ComposeError::Engine)
}

/// Runs whichever bounded maintenance tasks are due per `schedule`,
/// recording an updated `last_run` (in `Db`'s `META` table) for each task
/// it actually ran. `scope` is the default read/write scope: `Purge` and
/// `Lifecycle` build their `ScopeSet` from it via
/// `topodb_json::scope_to_scope_set`; `Compact` is scope-agnostic (it
/// operates on the whole op log).
///
/// `Compact`, `Purge`, and `Lifecycle` are bounded, so they always run
/// when due. `Reingest` is heavy: unless `allow_heavy` is set, a due
/// `Reingest` is pushed to `report.deferred` and its `last_run` is left
/// untouched, so the next catch-up call (e.g. once a background daemon is
/// willing to pay the heavy cost) still sees it as due.
pub fn run_catch_up(
    db: &Db,
    scope: Scope,
    schedule: &Schedule,
    now_ms: i64,
    allow_heavy: bool,
) -> Result<CatchUpReport, ComposeError> {
    let last_run = |task: Task| get_last_run(db, task).unwrap_or(None);
    let due = due_tasks(schedule, &last_run, now_ms);

    let mut report = CatchUpReport::default();
    let scopes = topodb_json::scope_to_scope_set(scope);

    // `Reingest` gets bespoke handling rather than folding into the `due`
    // loop below: while it's off by default (`Schedule::defaults()` has it
    // disabled — nothing to re-ingest until a source is configured), a
    // catch-up call with `!allow_heavy` still needs to surface "heavy work
    // is being skipped" to the caller so a daemon/CLI can decide whether to
    // re-run with `allow_heavy: true`. So the defer signal is unconditional
    // on `allow_heavy`, independent of whether the schedule/interval would
    // otherwise call it due; only the *run* path (allow_heavy == true)
    // respects `due` (and thus `enabled`), since running the (stub) heavy
    // task before it's configured/scheduled would be pointless.
    if !allow_heavy {
        report.deferred.push(Task::Reingest);
    } else if due.contains(&Task::Reingest) {
        // TODO(reingest): wire obsidian/OKF refresh
        set_last_run(db, Task::Reingest, now_ms)?;
        report.ran.push(Task::Reingest);
    }

    for task in due.into_iter().filter(|t| *t != Task::Reingest) {
        match task {
            Task::Compact => {
                let current = db.current_seq().map_err(ComposeError::Engine)?;
                let keep_from = current.saturating_sub(RETAIN);
                match db.compact_ops(keep_from) {
                    Ok(()) => {}
                    // Already below the retained floor: nothing to do,
                    // treat as a no-op success rather than a failure.
                    Err(topodb::TopoError::Compacted { .. }) => {}
                    Err(e) => return Err(ComposeError::Engine(e)),
                }
                set_last_run(db, task, now_ms)?;
                report.ran.push(task);
            }
            Task::Purge => {
                let tombstoned_before = now_ms - PURGE_GRACE_MS;
                // `plan_purge` requires a positive unix-ms cutoff. Before
                // the epoch + grace period has elapsed (e.g. early clock
                // values in tests), nothing could possibly qualify yet —
                // skip planning rather than erroring, and still record the
                // run (it's a legitimately empty catch-up, not a failure).
                if tombstoned_before > 0 {
                    let (ops, _ids) = topodb_json::plan_purge(db, &scopes, tombstoned_before)?;
                    if !ops.is_empty() {
                        db.submit(ops).map_err(ComposeError::Engine)?;
                    }
                }
                set_last_run(db, task, now_ms)?;
                report.ran.push(task);
            }
            Task::Lifecycle => {
                let params = LifecycleParams::default();
                let candidates = topodb_json::lifecycle_candidates(db, &scopes, &params, now_ms)?;
                report.surfaced_candidates = candidates.len();
                db.set_meta(
                    "onboarding:lifecycle_candidates",
                    candidates.len().to_string().as_bytes(),
                )
                .map_err(ComposeError::Engine)?;
                set_last_run(db, task, now_ms)?;
                report.ran.push(task);
            }
            // Handled above, before this loop — filtered out of `due` here.
            Task::Reingest => unreachable!("Reingest is filtered out before this loop"),
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Schedule;
    use topodb::Db;

    #[test]
    fn due_when_never_run_and_enabled() {
        let s = Schedule::defaults();
        let never = |_t: Task| None;
        let due = due_tasks(&s, &never, 1_000_000);
        assert!(due.contains(&Task::Compact));
        assert!(!due.contains(&Task::Reingest)); // disabled by default
    }

    #[test]
    fn not_due_within_interval() {
        let s = Schedule::defaults();
        let now = 1_000_000_000i64;
        let recent = |_t: Task| Some(now - 10_000); // 10s ago, interval is a day
        assert!(due_tasks(&s, &recent, now).is_empty());
    }

    #[test]
    fn catch_up_runs_compact_and_advances_last_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(&dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let scope = Scope::Shared;
        let now = 1_000_000_000i64;
        let rep = run_catch_up(&db, scope, &Schedule::defaults(), now, false).unwrap();
        assert!(rep.ran.contains(&Task::Compact));
        assert!(rep.deferred.contains(&Task::Reingest));
        let lr = db.get_meta(Task::Compact.meta_key()).unwrap().unwrap();
        assert_eq!(String::from_utf8(lr).unwrap().parse::<i64>().unwrap(), now);
    }
}
