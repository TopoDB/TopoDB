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
//!
//! Warehouse tasks (WarehouseDrain, WarehouseDerive, WarehouseTier) run only
//! when a warehouse is provided; WarehouseDerive is heavy (deferred unless
//! `allow_heavy` is set); errors are captured but do not fail the catch-up.

use std::path::PathBuf;
use topodb::{Db, Scope};
use topodb_json::{ComposeError, LifecycleParams};
use topodb_warehouse::{
    DeriveReport, DrainReport, MirrorReport, TierReport, Warehouse, WarehouseConfig,
};

use crate::config::Schedule;

/// Handle to a warehouse directory and config, plus optional embedder function.
pub struct HygieneWarehouse<'a> {
    pub dir: PathBuf,
    pub config: WarehouseConfig,
    pub embed: Option<topodb_warehouse::EmbedFn<'a>>,
}

/// Result of a warehouse hygiene run: drain, mirror, derive, and tier reports
/// plus any errors encountered (which don't fail the catch-up).
#[derive(Debug, Clone, Default)]
pub struct WarehouseHygieneReport {
    pub drain: Option<DrainReport>,
    pub mirror: Option<MirrorReport>,
    pub derive: Option<DeriveReport>,
    pub tier: Option<TierReport>,
    pub errors: Vec<String>,
}

/// One bounded (or heavy) maintenance task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Task {
    WarehouseDrain,
    Compact,
    Purge,
    Reingest,
    Lifecycle,
    WarehouseDerive,
    WarehouseTier,
}

impl Task {
    /// The `META` key under which this task's last-run timestamp (ASCII
    /// decimal `i64` ms) is stored.
    pub fn meta_key(&self) -> &'static str {
        match self {
            Task::WarehouseDrain => "onboarding:last_run:warehouse_drain",
            Task::Compact => "onboarding:last_run:compact",
            Task::Purge => "onboarding:last_run:purge",
            Task::Reingest => "onboarding:last_run:reingest",
            Task::Lifecycle => "onboarding:last_run:lifecycle",
            Task::WarehouseDerive => "onboarding:last_run:warehouse_derive",
            Task::WarehouseTier => "onboarding:last_run:warehouse_tier",
        }
    }

    fn all() -> [Task; 7] {
        [
            Task::WarehouseDrain,
            Task::Compact,
            Task::Purge,
            Task::Reingest,
            Task::Lifecycle,
            Task::WarehouseDerive,
            Task::WarehouseTier,
        ]
    }

    fn schedule_entry(&self, schedule: &Schedule) -> crate::config::ScheduleEntry {
        match self {
            Task::WarehouseDrain => schedule.warehouse_drain,
            Task::Compact => schedule.compact,
            Task::Purge => schedule.purge,
            Task::Reingest => schedule.reingest,
            Task::Lifecycle => schedule.lifecycle,
            Task::WarehouseDerive => schedule.warehouse_derive,
            Task::WarehouseTier => schedule.warehouse_tier,
        }
    }
}

/// The outcome of one `run_catch_up` call.
#[derive(Debug, Clone, Default)]
pub struct CatchUpReport {
    /// Tasks that were due and actually ran (including the `Reingest` stub
    /// when `allow_heavy` was set, and warehouse tasks when a warehouse was
    /// provided).
    pub ran: Vec<Task>,
    /// Tasks that were due but deferred (currently `Reingest` when `!allow_heavy`,
    /// and `WarehouseDerive` when `!allow_heavy` and warehouse provided;
    /// a disabled/not-due task is skipped entirely, never deferred) — their
    /// `last_run` was NOT advanced, so they stay due until a caller runs catch-up
    /// with `allow_heavy: true`.
    pub deferred: Vec<Task>,
    /// Count of lifecycle decay candidates surfaced by the most recent
    /// `Lifecycle` run this call (0 if `Lifecycle` wasn't due/run).
    pub surfaced_candidates: usize,
    /// Per-source normalized reports from a `Reingest` run this call (empty
    /// unless `Reingest` was due and ran under `allow_heavy`).
    pub reingest: Vec<crate::reingest::ReingestReport>,
    /// Warehouse hygiene results (drain, mirror, derive, tier) from this
    /// call (None unless a warehouse was provided).
    pub warehouse: Option<WarehouseHygieneReport>,
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
/// when due. `Reingest` is heavy: a disabled/not-due `Reingest` is skipped
/// entirely (neither ran nor deferred); a DUE `Reingest` is deferred
/// unless `allow_heavy` is set — with `!allow_heavy` it's pushed to
/// `report.deferred` and its `last_run` is left untouched, so the next
/// catch-up call (e.g. once a background daemon is willing to pay the
/// heavy cost) still sees it as due.
///
/// Warehouse tasks (WarehouseDrain, WarehouseDerive, WarehouseTier) run only
/// when a warehouse is provided via `run_catch_up_with`. WarehouseDrain and
/// WarehouseTier are bounded; WarehouseDerive is heavy. Errors are captured
/// in the report but do not fail the catch-up.
pub fn run_catch_up(
    db: &Db,
    scope: Scope,
    schedule: &Schedule,
    sources: &[crate::reingest::ResolvedSource],
    now_ms: i64,
    allow_heavy: bool,
) -> Result<CatchUpReport, ComposeError> {
    run_catch_up_with(db, scope, schedule, sources, now_ms, allow_heavy, None)
}

/// Like `run_catch_up`, but with optional warehouse support. `warehouse` may
/// be provided to enable WarehouseDrain, WarehouseDerive, and WarehouseTier
/// tasks. When `warehouse.is_some()`, those tasks run according to their
/// schedule (or defer if heavy and `!allow_heavy`); Compact's `keep_from`
/// floor is clamped to never compact below the warehouse's mirrored_seq + 1.
pub fn run_catch_up_with(
    db: &Db,
    scope: Scope,
    schedule: &Schedule,
    sources: &[crate::reingest::ResolvedSource],
    now_ms: i64,
    allow_heavy: bool,
    warehouse: Option<&HygieneWarehouse<'_>>,
) -> Result<CatchUpReport, ComposeError> {
    let last_run = |task: Task| get_last_run(db, task).unwrap_or(None);
    let due = due_tasks(schedule, &last_run, now_ms);

    let mut report = CatchUpReport::default();
    let scopes = topodb_json::scope_to_scope_set(scope);

    let open_wh = |w: &HygieneWarehouse<'_>| Warehouse::open(&w.dir, w.config.clone());

    // Handle warehouse tasks before Reingest, if a warehouse is provided.
    if let Some(w) = warehouse {
        let wr = report.warehouse.get_or_insert_with(Default::default);
        if due.contains(&Task::WarehouseDrain) {
            match open_wh(w) {
                Ok(mut wh) => {
                    match wh.drain(now_ms) {
                        Ok(r) => wr.drain = Some(r),
                        Err(e) => wr.errors.push(format!("drain: {e}")),
                    }
                    match wh.mirror(db, now_ms) {
                        Ok(r) => wr.mirror = Some(r),
                        Err(e) => wr.errors.push(format!("mirror: {e}")),
                    }
                    let _ = topodb_warehouse::report::set_last_run(db, "drain", now_ms);
                }
                Err(e) => wr.errors.push(format!("open: {e}")),
            }
            set_last_run(db, Task::WarehouseDrain, now_ms)?;
            report.ran.push(Task::WarehouseDrain);
        }
        if due.contains(&Task::WarehouseDerive) {
            if allow_heavy {
                match open_wh(w) {
                    Ok(wh) => match topodb_warehouse::derive(db, &wh, w.embed, now_ms, false) {
                        Ok(r) => wr.derive = Some(r),
                        Err(e) => wr.errors.push(format!("derive: {e}")),
                    },
                    Err(e) => wr.errors.push(format!("open: {e}")),
                }
                set_last_run(db, Task::WarehouseDerive, now_ms)?;
                report.ran.push(Task::WarehouseDerive);
            } else {
                report.deferred.push(Task::WarehouseDerive);
            }
        }
        if due.contains(&Task::WarehouseTier) {
            match open_wh(w) {
                Ok(mut wh) => match topodb_warehouse::tier(db, &mut wh, now_ms) {
                    Ok(r) => wr.tier = Some(r),
                    Err(e) => wr.errors.push(format!("tier: {e}")),
                },
                Err(e) => wr.errors.push(format!("open: {e}")),
            }
            set_last_run(db, Task::WarehouseTier, now_ms)?;
            report.ran.push(Task::WarehouseTier);
        }
    }

    // `Reingest` gets bespoke handling rather than folding into the `due`
    // loop below: it's off by default (`Schedule::defaults()` has it
    // disabled — nothing to re-ingest until a source is configured), and a
    // disabled/not-due `Reingest` is skipped entirely (neither ran nor
    // deferred). A DUE `Reingest` is deferred unless `allow_heavy` is set:
    // with `!allow_heavy` it's pushed to `report.deferred` and its
    // `last_run` is left untouched so it stays due for a later heavy run;
    // with `allow_heavy` it runs (the stub) and its `last_run` advances.
    if due.contains(&Task::Reingest) {
        if allow_heavy {
            report.reingest = crate::reingest::run_reingest(db, sources, scope, now_ms);
            set_last_run(db, Task::Reingest, now_ms)?;
            report.ran.push(Task::Reingest);
        } else {
            // heavy work deferred to a daemon/allow_heavy caller; last_run
            // left as-is
            report.deferred.push(Task::Reingest);
        }
    }

    for task in due.into_iter().filter(|t| {
        !matches!(
            t,
            Task::Reingest | Task::WarehouseDrain | Task::WarehouseDerive | Task::WarehouseTier
        )
    }) {
        match task {
            Task::Compact => {
                let current = db.current_seq().map_err(ComposeError::Engine)?;
                let mut keep_from = current.saturating_sub(RETAIN);
                // Clamp Compact to never go below the warehouse's mirrored_seq + 1.
                if warehouse.is_some() {
                    keep_from = keep_from
                        .min(topodb_warehouse::mirrored_seq(db).map_err(ComposeError::Engine)? + 1);
                }
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
                // Stash the candidate ids (not just the count) so an agent can
                // review them without re-running `lifecycle_candidates`. ULIDs
                // are fixed-width and comma-free, so a comma-joined list
                // round-trips unambiguously (empty when there are none).
                let ids = candidates
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                db.set_meta("onboarding:lifecycle_candidate_ids", ids.as_bytes())
                    .map_err(ComposeError::Engine)?;
                set_last_run(db, task, now_ms)?;
                report.ran.push(task);
            }
            // Handled above, before this loop — filtered out of `due` here.
            Task::Reingest | Task::WarehouseDrain | Task::WarehouseDerive | Task::WarehouseTier => {
                unreachable!("these tasks are filtered out before this loop")
            }
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
        let due = due_tasks(&s, &recent, now);
        // Only warehouse_drain should be due (interval 0); all others are deferred.
        assert_eq!(due.len(), 1);
        assert!(due.contains(&Task::WarehouseDrain));
    }

    #[test]
    fn catch_up_runs_compact_and_advances_last_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let scope = Scope::Shared;
        let now = 1_000_000_000i64;
        let rep = run_catch_up(&db, scope, &Schedule::defaults(), &[], now, false).unwrap();
        assert!(rep.ran.contains(&Task::Compact));
        assert!(!rep.deferred.contains(&Task::Reingest)); // disabled by default => skipped
        let lr = db.get_meta(Task::Compact.meta_key()).unwrap().unwrap();
        assert_eq!(String::from_utf8(lr).unwrap().parse::<i64>().unwrap(), now);
    }

    #[test]
    fn catch_up_stashes_lifecycle_candidate_ids_not_just_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let now = 1_000_000_000i64;
        // Lifecycle is enabled by default, so it runs on this catch-up.
        run_catch_up(&db, Scope::Shared, &Schedule::defaults(), &[], now, false).unwrap();
        // Both the count and the id list are stashed (empty id list on a fresh
        // store, but the key is present so a consumer can read it).
        assert!(db
            .get_meta("onboarding:lifecycle_candidates")
            .unwrap()
            .is_some());
        assert!(db
            .get_meta("onboarding:lifecycle_candidate_ids")
            .unwrap()
            .is_some());
    }

    #[test]
    fn catch_up_defers_due_reingest_then_runs_with_allow_heavy() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let scope = Scope::Shared;
        let now = 1_000_000_000i64;

        let mut schedule = Schedule::defaults();
        schedule.reingest.enabled = true;

        // A source pointing at an empty (but existing) vault dir: reingest runs
        // cleanly, zero memories, no errors.
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        let sources = vec![crate::reingest::ResolvedSource {
            kind: crate::config::SourceKind::Obsidian,
            path: vault,
            scope: None,
        }];

        let rep = run_catch_up(&db, scope, &schedule, &sources, now, false).unwrap();
        assert!(rep.deferred.contains(&Task::Reingest));
        assert!(rep.reingest.is_empty());
        assert!(db.get_meta(Task::Reingest.meta_key()).unwrap().is_none());

        let rep = run_catch_up(&db, scope, &schedule, &sources, now, true).unwrap();
        assert!(rep.ran.contains(&Task::Reingest));
        assert_eq!(rep.reingest.len(), 1);
        assert!(db.get_meta(Task::Reingest.meta_key()).unwrap().is_some());
    }

    #[test]
    fn reingest_advances_last_run_even_when_a_source_path_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let now = 1_000_000_000i64;
        let mut schedule = Schedule::defaults();
        schedule.reingest.enabled = true;
        let sources = vec![crate::reingest::ResolvedSource {
            kind: crate::config::SourceKind::Okf,
            path: dir.path().join("nope"),
            scope: None,
        }];
        let rep = run_catch_up(&db, Scope::Shared, &schedule, &sources, now, true).unwrap();
        assert!(rep.ran.contains(&Task::Reingest));
        assert_eq!(rep.reingest.len(), 1);
        assert_eq!(rep.reingest[0].errors.len(), 1);
        assert!(db.get_meta(Task::Reingest.meta_key()).unwrap().is_some());
    }

    fn wh_dir(dir: &std::path::Path) -> HygieneWarehouse<'static> {
        HygieneWarehouse {
            dir: dir.join("w"),
            config: topodb_warehouse::WarehouseConfig {
                spool_min_age_ms: 0,
                ..Default::default()
            },
            embed: None,
        }
    }

    #[test]
    fn warehouse_tasks_skip_without_warehouse_and_run_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        let now = 1_000_000_000i64;
        let rep = run_catch_up(&db, Scope::Shared, &Schedule::defaults(), &[], now, true).unwrap();
        assert!(!rep.ran.contains(&Task::WarehouseDrain) && rep.warehouse.is_none());
        let wh = wh_dir(dir.path());
        let rep = run_catch_up_with(
            &db,
            Scope::Shared,
            &Schedule::defaults(),
            &[],
            now,
            false,
            Some(&wh),
        )
        .unwrap();
        assert!(rep.ran.contains(&Task::WarehouseDrain));
        assert!(rep.deferred.contains(&Task::WarehouseDerive));
        assert!(rep.ran.contains(&Task::WarehouseTier));
        let w = rep.warehouse.as_ref().unwrap();
        assert!(
            w.drain.is_some() && w.mirror.is_some() && w.derive.is_none() && w.errors.is_empty()
        );
        assert!(dir.path().join("w").join("MANIFEST.json").is_file());
        // drain is due again immediately (interval 0); derive runs under allow_heavy
        let rep2 = run_catch_up_with(
            &db,
            Scope::Shared,
            &Schedule::defaults(),
            &[],
            now + 1,
            true,
            Some(&wh),
        )
        .unwrap();
        assert!(
            rep2.ran.contains(&Task::WarehouseDrain) && rep2.ran.contains(&Task::WarehouseDerive)
        );
        assert!(rep2.warehouse.as_ref().unwrap().derive.is_some());
        assert!(db
            .get_meta(Task::WarehouseDerive.meta_key())
            .unwrap()
            .is_some());
    }

    #[test]
    fn compact_is_clamped_to_mirrored_seq() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("m.redb"), topodb_json::default_spec()).unwrap();
        for _ in 0..(RETAIN as usize + 50) {
            let mut p = topodb::Props::new();
            p.insert("content".into(), topodb::PropValue::Str("x".into()));
            db.submit(vec![topodb::Op::CreateNode {
                id: topodb::NodeId::new(),
                scope: Scope::Shared,
                label: "Memory".into(),
                props: p,
            }])
            .unwrap();
        }
        let now = 1_000_000_000i64;
        let wh = wh_dir(dir.path());
        // Pin the watermark low and keep drain OFF so it stays there: Compact
        // (due, never run) wants keep_from = current - RETAIN = 50 but must
        // stop at mirrored_seq + 1 = 11.
        db.set_meta(topodb_warehouse::MIRRORED_SEQ_KEY, b"10")
            .unwrap();
        let mut sched = Schedule::defaults();
        sched.warehouse_drain.enabled = false;
        let rep =
            run_catch_up_with(&db, Scope::Shared, &sched, &[], now, false, Some(&wh)).unwrap();
        assert!(rep.ran.contains(&Task::Compact));
        assert!(matches!(
            db.ops_since(10),
            Err(topodb::TopoError::Compacted { .. })
        ));
        assert!(
            db.ops_since(11).is_ok(),
            "clamp held: nothing at/after mirrored_seq+1 was compacted"
        );
        // Now let drain mirror the tail; the watermark catches up and the next
        // compaction may proceed to the RETAIN floor.
        sched.warehouse_drain.enabled = true;
        sched.compact.interval_secs = 0;
        run_catch_up_with(&db, Scope::Shared, &sched, &[], now + 10, false, Some(&wh)).unwrap();
        assert_eq!(
            topodb_warehouse::mirrored_seq(&db).unwrap(),
            db.current_seq().unwrap()
        );
        assert!(matches!(
            db.ops_since(11),
            Err(topodb::TopoError::Compacted { .. })
        ));
        assert!(db.ops_since(db.current_seq().unwrap() - RETAIN + 1).is_ok());
    }
}
