//! `topodb warehouse …` handlers. Thin over topodb-warehouse + onboarding config.
use crate::cli::WarehouseCommand;
use crate::output;
use std::path::{Path, PathBuf};
use topodb::Db;
use topodb_warehouse::{Warehouse, WarehouseConfig};

pub fn nearest_topodb_toml(from: &Path) -> Option<PathBuf> {
    let mut dir = Some(from);
    while let Some(d) = dir {
        let c = d.join(".topodb.toml");
        if c.is_file() {
            return Some(c);
        }
        dir = d.parent();
    }
    None
}

/// Resolve (dir, config) for `db_path`; exits 2 when the warehouse is disabled.
pub fn resolve(db_path: &Path) -> (PathBuf, WarehouseConfig) {
    let start = db_path.parent().unwrap_or_else(|| Path::new("."));
    let (cfg, base) = match nearest_topodb_toml(start) {
        Some(p) => (
            topodb_onboarding::parse(&std::fs::read_to_string(&p).unwrap_or_default()),
            p.parent().unwrap_or(start).to_path_buf(),
        ),
        None => (topodb_onboarding::parse(""), start.to_path_buf()),
    };
    match topodb_onboarding::resolve_warehouse(
        db_path,
        &cfg.warehouse,
        &base,
        std::env::var("TOPODB_WAREHOUSE_DIR").ok().as_deref(),
        std::env::var("TOPODB_WAREHOUSE").ok().as_deref(),
    ) {
        Some(x) => x,
        None => output::fail(
            "rejected",
            "warehouse disabled ([warehouse].enabled=false or TOPODB_WAREHOUSE=0)",
            2,
        ),
    }
}
fn open(db_path: &Path) -> Warehouse {
    let (dir, cfg) = resolve(db_path);
    match Warehouse::open(&dir, cfg) {
        Ok(w) => w,
        Err(e) => output::fail(
            "rejected",
            &format!("open warehouse {}: {e}", dir.display()),
            2,
        ),
    }
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
fn json<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).expect("report serializes")
}

/// Commands that never open the db (run before the direct open in main).
pub fn run_dbless(cmd: &WarehouseCommand, db_path: &Path, pretty: bool) {
    match cmd {
        WarehouseCommand::Status => {
            let wh = open(db_path);
            match wh.status(None) {
                Ok(s) => output::ok(&json(&s), pretty),
                Err(e) => output::fail("rejected", &e.to_string(), 2),
            }
        }
        WarehouseCommand::Verify => {
            let wh = open(db_path);
            output::ok(
                &serde_json::json!({ "problems": topodb_warehouse::segment::verify(&wh.layout, &wh.manifest) }),
                pretty,
            )
        }
        WarehouseCommand::Show { hash } => {
            let wh = open(db_path);
            if let Ok(Some(b)) = topodb_warehouse::blob::get_blob(&wh.layout, hash) {
                output::ok(
                    &serde_json::json!({ "hash": hash, "text": String::from_utf8_lossy(&b) }),
                    pretty,
                )
            }
            match wh.events() {
                Ok(evs) => {
                    for ev in evs {
                        if let Some(a) = ev.artifact {
                            if a.hash.as_deref() == Some(hash.as_str()) {
                                if let Some(c) = a.content {
                                    output::ok(
                                        &serde_json::json!({ "hash": hash, "text": c }),
                                        pretty,
                                    )
                                }
                            }
                        }
                    }
                    output::fail(
                        "rejected",
                        &format!("no stored text for {hash} (pointer-only, expired, or unknown)"),
                        2,
                    )
                }
                Err(e) => output::fail("rejected", &e.to_string(), 2),
            }
        }
        _ => {}
    }
}

/// Commands that need the open db.
pub fn run(cmd: &WarehouseCommand, db: &Db, db_path: &Path, pretty: bool) -> ! {
    let mut wh = open(db_path);
    let now = now_ms();
    match cmd {
        WarehouseCommand::Drain => {
            let drain = wh
                .drain(now)
                .unwrap_or_else(|e| output::fail("rejected", &format!("drain: {e}"), 2));
            let mirror = wh
                .mirror(db, now)
                .unwrap_or_else(|e| output::fail("rejected", &format!("mirror: {e}"), 2));
            let _ = topodb_warehouse::report::set_last_run(db, "drain", now);
            output::ok(
                &serde_json::json!({ "drain": json(&drain), "mirror": json(&mirror) }),
                pretty,
            )
        }
        WarehouseCommand::Derive { rederive } => {
            match topodb_warehouse::derive(db, &wh, None, now, *rederive) {
                Ok(r) => output::ok(&json(&r), pretty),
                Err(e) => output::fail("rejected", &format!("derive: {e}"), 2),
            }
        }
        WarehouseCommand::Tier => match topodb_warehouse::tier(db, &mut wh, now) {
            Ok(r) => output::ok(&json(&r), pretty),
            Err(e) => output::fail("rejected", &format!("tier: {e}"), 2),
        },
        WarehouseCommand::Rebuild { out } => {
            match topodb_warehouse::rebuild(&wh, out, topodb_json::default_spec()) {
                Ok(r) => output::ok(&json(&r), pretty),
                Err(e) => output::fail("rejected", &format!("rebuild: {e}"), 2),
            }
        }
        WarehouseCommand::Status | WarehouseCommand::Verify | WarehouseCommand::Show { .. } => {
            unreachable!("handled db-less")
        }
    }
}
