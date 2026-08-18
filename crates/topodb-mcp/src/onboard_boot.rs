//! Server-startup onboarding trigger.
//!
//! `topodb-mcp` is the process EVERY code client (Claude Code plugin, pi,
//! ...) spawns, so its boot is the one client-agnostic place to (1) ensure
//! a `CONVENTIONS.md` sits next to the db and (2) run overdue hygiene
//! catch-up — the PRIMARY hygiene path, since the resident daemon idle-exits
//! in ~60s and can't be relied on for a background schedule.
//!
//! Strictly best-effort: nothing here may fail or slow server boot. Every
//! error (including a panic from either step) is swallowed, with at most a
//! stderr note — never a `Result` that propagates to `main`.

use std::path::{Path, PathBuf};

use topodb::{Db, Scope};

/// Resolved hygiene configuration: schedule, sources, and optional warehouse.
pub(crate) struct ResolvedHygiene {
    pub schedule: topodb_onboarding::Schedule,
    pub sources: Vec<topodb_onboarding::ResolvedSource>,
    pub warehouse: Option<(PathBuf, topodb_warehouse::WarehouseConfig)>,
}

/// Runs onboarding's boot-time side effects against an already-open `db`.
/// Never panics or returns an error to the caller.
pub fn run_boot_onboarding(db: &Db, db_path: &Path, scope: Scope, now_ms: i64) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ensure_conventions(db_path);
        run_hygiene_catch_up(db, db_path, scope, now_ms);
    }));
    if result.is_err() {
        eprintln!("topodb-mcp: boot onboarding panicked (ignored)");
    }
}

fn ensure_conventions(db_path: &Path) {
    if let Some(dir) = db_path.parent() {
        if let Err(e) = topodb_onboarding::ensure_conventions_file(dir) {
            eprintln!("topodb-mcp: boot onboarding: writing CONVENTIONS.md: {e}");
        }
    }
}

fn hygiene_warehouse<'a>(
    r: &ResolvedHygiene,
    embed: Option<topodb_warehouse::EmbedFn<'a>>,
) -> Option<topodb_onboarding::HygieneWarehouse<'a>> {
    r.warehouse
        .as_ref()
        .map(|(dir, config)| topodb_onboarding::HygieneWarehouse {
            dir: dir.clone(),
            config: config.clone(),
            embed,
        })
}

fn run_hygiene_catch_up(db: &Db, db_path: &Path, scope: Scope, now_ms: i64) {
    let r = resolve_config(db_path);
    let wh = hygiene_warehouse(&r, None);
    if let Err(e) = topodb_onboarding::run_catch_up_with(
        db,
        scope,
        &r.schedule,
        &r.sources,
        now_ms,
        false,
        wh.as_ref(),
    ) {
        eprintln!("topodb-mcp: boot onboarding: hygiene catch-up: {e:?}");
    }
    // Ensure warehouse mirroring is initialized even on fresh databases
    if wh.is_some()
        && db
            .get_meta(topodb_warehouse::MIRRORED_SEQ_KEY)
            .unwrap_or_default()
            .is_none()
    {
        let _ = db.set_meta(topodb_warehouse::MIRRORED_SEQ_KEY, b"0");
    }
}

/// Resolves the hygiene schedule, sources, and warehouse configuration for a db:
/// the nearest `.topodb.toml` walking up from `db_path`'s parent, parsed for
/// its `[schedule]` table, `[[reingest.source]]` array (source paths
/// resolved against the toml's own directory), and `[warehouse]` section
/// (path resolved relative to toml), or defaults if none is found or
/// it fails to parse/read. Shared by the boot catch-up and the resident
/// daemon's background tick (`daemon::serve`), so the two paths never drift.
pub(crate) fn resolve_config(db_path: &Path) -> ResolvedHygiene {
    let env_dir = std::env::var("TOPODB_WAREHOUSE_DIR").ok();
    let env_switch = std::env::var("TOPODB_WAREHOUSE").ok();
    let (cfg, base_dir) = match nearest_topodb_toml(db_path) {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(text) => (
                topodb_onboarding::parse(&text),
                p.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| Path::new(".").to_path_buf()),
            ),
            Err(_) => (
                topodb_onboarding::parse(""),
                db_path.parent().map(Path::to_path_buf).unwrap_or_default(),
            ),
        },
        None => (
            topodb_onboarding::parse(""),
            db_path.parent().map(Path::to_path_buf).unwrap_or_default(),
        ),
    };
    let sources = topodb_onboarding::resolve_sources(
        &base_dir,
        topodb_onboarding::env_home().as_deref(),
        &cfg.sources,
    );
    let warehouse = topodb_onboarding::resolve_warehouse(
        db_path,
        &cfg.warehouse,
        &base_dir,
        env_dir.as_deref(),
        env_switch.as_deref(),
    );
    ResolvedHygiene {
        schedule: cfg.schedule,
        sources,
        warehouse,
    }
}

/// The resident daemon's periodic hygiene tick body — the same catch-up as
/// boot, but with `allow_heavy = true` so the heavy re-ingest work the inline
/// boot path defers can run while the daemon happens to be alive. Genuinely
/// best-effort: every error is swallowed (never crashes the daemon), and a
/// tick with nothing due (gated by each task's `last_run` META) is a no-op.
pub(crate) fn tick_once(
    db: &Db,
    scope: Scope,
    r: &ResolvedHygiene,
    embed: Option<topodb_warehouse::EmbedFn<'_>>,
    now_ms: i64,
) {
    let wh = hygiene_warehouse(r, embed);
    let _ = topodb_onboarding::run_catch_up_with(
        db,
        scope,
        &r.schedule,
        &r.sources,
        now_ms,
        true,
        wh.as_ref(),
    );
}

/// Walks from `db_path`'s parent directory upward looking for the nearest
/// `.topodb.toml`. Mirrors `topodb-cli`'s project-config resolution, but
/// kept as a tiny local walk here rather than a cross-crate dependency on
/// `topodb-cli` (which is a binary crate, not a library other crates
/// should link against).
fn nearest_topodb_toml(db_path: &Path) -> Option<std::path::PathBuf> {
    let mut dir = db_path.parent()?;
    loop {
        let candidate = dir.join(".topodb.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn writes_conventions_and_advances_hygiene() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.redb");
        let db = Db::open_with(&db_path, topodb_json::default_spec()).unwrap();

        assert!(db
            .get_meta("onboarding:last_run:compact")
            .unwrap()
            .is_none());

        run_boot_onboarding(&db, &db_path, Scope::Shared, now_ms());

        assert!(dir.path().join("CONVENTIONS.md").is_file());
        assert!(db
            .get_meta("onboarding:last_run:compact")
            .unwrap()
            .is_some());
    }

    #[test]
    fn is_a_noop_on_repeat_calls_within_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.redb");
        let db = Db::open_with(&db_path, topodb_json::default_spec()).unwrap();

        let t0 = now_ms();
        run_boot_onboarding(&db, &db_path, Scope::Shared, t0);
        let first_run = db.get_meta("onboarding:last_run:compact").unwrap();

        // Calling again moments later must not panic or error even though
        // nothing is due yet.
        run_boot_onboarding(&db, &db_path, Scope::Shared, t0 + 1);
        let second_run = db.get_meta("onboarding:last_run:compact").unwrap();
        assert_eq!(first_run, second_run);
    }

    #[test]
    fn tick_once_advances_last_run_then_is_gated_on_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.redb");
        let db = Db::open_with(&db_path, topodb_json::default_spec()).unwrap();
        let resolved = ResolvedHygiene {
            schedule: topodb_onboarding::Schedule::defaults(),
            sources: vec![],
            warehouse: None,
        };

        assert!(db
            .get_meta("onboarding:last_run:compact")
            .unwrap()
            .is_none());

        let t0 = now_ms();
        tick_once(&db, Scope::Shared, &resolved, None, t0);
        let first_run = db.get_meta("onboarding:last_run:compact").unwrap();
        assert!(first_run.is_some());

        // Same instant again: nothing new is due, so this must be a no-op.
        tick_once(&db, Scope::Shared, &resolved, None, t0);
        let second_run = db.get_meta("onboarding:last_run:compact").unwrap();
        assert_eq!(second_run, first_run);
    }

    #[test]
    fn resolve_config_falls_back_to_defaults_without_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.redb");
        let r = resolve_config(&db_path);
        assert_eq!(r.schedule, topodb_onboarding::Schedule::defaults());
        assert!(r.sources.is_empty());
    }

    #[test]
    fn resolve_config_finds_nearest_topodb_toml() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".topodb.toml"), "").unwrap();
        let nested = root.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let db_path = nested.join("memory.redb");

        // An empty config parses to the default schedule too, but exercises
        // the walk-and-parse path rather than the "no file found" path.
        let r = resolve_config(&db_path);
        assert_eq!(r.schedule, topodb_onboarding::Schedule::defaults());
    }

    #[test]
    fn resolve_config_returns_sources_resolved_against_toml_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".topodb.toml"),
            "[[reingest.source]]\nkind = \"okf\"\npath = \"./bundle\"\n",
        )
        .unwrap();
        let db_path = root.path().join("memory.redb");

        let r = resolve_config(&db_path);
        assert_eq!(r.sources.len(), 1);
        assert_eq!(r.sources[0].path, root.path().join("./bundle"));
    }

    #[test]
    fn nearest_topodb_toml_walks_up_from_db_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".topodb.toml"), "").unwrap();
        let nested = root.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let db_path = nested.join("memory.redb");

        let found = nearest_topodb_toml(&db_path).unwrap();
        assert_eq!(found, root.path().join(".topodb.toml"));
    }

    #[test]
    fn boot_drains_and_mirrors_into_warehouse_next_to_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.redb");
        let db = Db::open_with(&db_path, topodb_json::default_spec()).unwrap();
        run_boot_onboarding(&db, &db_path, Scope::Shared, 1_000_000_000);
        assert!(dir
            .path()
            .join("memory.warehouse")
            .join("MANIFEST.json")
            .is_file());
        assert!(db
            .get_meta(topodb_warehouse::MIRRORED_SEQ_KEY)
            .unwrap()
            .is_some());
    }

    #[test]
    fn resolve_config_reads_warehouse_section_and_env_switch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".topodb.toml"),
            "[warehouse]\nenabled = false\n",
        )
        .unwrap();
        let db_path = dir.path().join("memory.redb");
        assert!(resolve_config(&db_path).warehouse.is_none());
        std::fs::write(
            dir.path().join(".topodb.toml"),
            "[warehouse]\npath = \"wh\"\n",
        )
        .unwrap();
        let r = resolve_config(&db_path);
        assert_eq!(r.warehouse.as_ref().unwrap().0, dir.path().join("wh"));
    }
}
