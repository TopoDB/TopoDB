#![cfg(unix)]

//! Output-parity between the CLI's two execution paths.
//!
//! A `topodb` invocation runs one of two ways against the same database:
//! direct (open the file in-process) or socket-routed (a resident
//! `topodb-mcp --socket` daemon already holds the lock, so the CLI issues the
//! equivalent MCP tool call). An agent that shells out to `topodb` must get
//! byte-identical output whichever path runs — otherwise a script like
//! `topodb find ... | jq '.[0]'` works alone but breaks the moment a session's
//! daemon is resident. This test builds one fixture DB, captures each routed
//! read command's output against a live daemon, and asserts it matches the
//! direct output after normalizing only genuinely volatile fields (access
//! counters, wall-clock-derived timestamps/staleness, and traverse's
//! nondeterministic edge order).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// The `topodb-mcp` daemon binary sits beside the `topodb` CLI in the same
/// target dir. `cargo test -p topodb-cli` in isolation may not have built it,
/// so callers skip when it is absent (the workspace test gate builds it first).
fn daemon_bin() -> Option<PathBuf> {
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_topodb"));
    let bin = cli.parent()?.join("topodb-mcp");
    bin.exists().then_some(bin)
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_topodb"))
}

/// Run a `topodb --db <db> <args...>` invocation and return trimmed stdout.
fn run(db: &Path, args: &[&str]) -> String {
    let out = cli()
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("spawn topodb");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Kills the daemon child on drop so a failed assertion never leaks a lock
/// holder (the Windows-CI leaked-child lesson applies on unix too).
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the daemon and wait until `topodb daemon status` reports it live.
fn start_daemon(bin: &Path, db: &Path) -> DaemonGuard {
    let child = Command::new(bin)
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
    panic!("daemon never became live");
}

/// Replace values that legitimately differ between two runs (or two paths)
/// with a placeholder, so the comparison is of STRUCTURE, not of wall-clock
/// state. Reads bump access counters; `staleness` and `last_accessed_at`
/// derive from "now"; `current_seq` advances.
fn normalize(s: &str) -> String {
    let mut v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return s.to_string(),
    };
    scrub(&mut v);
    v.to_string()
}

fn scrub(v: &mut serde_json::Value) {
    const VOLATILE: &[&str] = &[
        "access_count",
        "last_accessed_at",
        "recorded_at",
        "created_at",
        "current_seq",
        "staleness",
        "score",
    ];
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if VOLATILE.contains(&k.as_str()) {
                    *val = serde_json::Value::Null;
                } else {
                    scrub(val);
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

/// Traverse's subgraph edge/node order is nondeterministic (it differs between
/// two direct runs too), so compare the SORTED multiset of normalized elements.
fn sorted_subgraph(s: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(s).expect("traverse json");
    scrub(&mut v);
    if let Some(sg) = v.get_mut("subgraph") {
        for key in ["edges", "nodes"] {
            if let Some(serde_json::Value::Array(a)) = sg.get_mut(key) {
                a.sort_by_key(|e| e.to_string());
            }
        }
    }
    v.to_string()
}

#[test]
fn routed_reads_match_direct_output() {
    let Some(bin) = daemon_bin() else {
        eprintln!("skipping: target topodb-mcp not built (run the workspace build first)");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("parity.redb");

    // Fixture: two memories, two entities, three edges — enough that a wrapper
    // or shape divergence in any list-returning read would show.
    let mem: String = {
        let out = run(
            &db,
            &["remember", "--content", "alpha fact about foxes", "--entity", "Fox"],
        );
        let v: serde_json::Value = serde_json::from_str(&out).expect("remember json");
        v["memory_id"].as_str().expect("memory id").to_string()
    };
    run(
        &db,
        &[
            "remember",
            "--content",
            "beta fact about foxes and dens",
            "--entity",
            "Fox",
            "--entity",
            "Den",
        ],
    );

    let ent: String = {
        let out = run(&db, &["find", "--label", "Entity", "--prop", "name", "--value", "Fox"]);
        let v: serde_json::Value = serde_json::from_str(&out).expect("find json");
        v[0]["id"].as_str().expect("entity id").to_string()
    };

    // Every routable read command, with args that exercise its output shape.
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("get", vec!["get", &ent]),
        ("find", vec!["find", "--label", "Entity", "--prop", "name", "--value", "Fox"]),
        ("get-edges", vec!["get-edges", &ent]),
        ("stats", vec!["stats", &mem]),
        ("lifecycle-candidates", vec!["lifecycle-candidates"]),
        ("traverse", vec!["traverse", &ent, "--max-hops", "2"]),
    ];

    // Direct outputs first (no daemon holds the lock yet).
    let direct: Vec<String> = cases.iter().map(|(_, a)| run(&db, a)).collect();

    // Now serve the SAME file from a daemon and route the same commands.
    let _daemon = start_daemon(&bin, &db);
    let routed: Vec<String> = cases.iter().map(|(_, a)| run(&db, a)).collect();

    // Stop the daemon before assertions so a failure can't leave it holding
    // the lock for later tests in the same binary.
    let stop = run(&db, &["daemon", "stop"]);
    assert!(
        serde_json::from_str::<serde_json::Value>(&stop)
            .ok()
            .and_then(|v| v.get("stopped").and_then(|b| b.as_bool()))
            == Some(true),
        "daemon stop did not confirm: {stop}"
    );

    for ((name, _), (d, r)) in cases.iter().zip(direct.iter().zip(routed.iter())) {
        if *name == "traverse" {
            assert_eq!(
                sorted_subgraph(d),
                sorted_subgraph(r),
                "traverse subgraph parity (sorted): direct={d}\nrouted={r}"
            );
        } else {
            assert_eq!(
                normalize(d),
                normalize(r),
                "{name} parity: direct={d}\nrouted={r}"
            );
        }
    }
}

#[test]
fn search_stays_available_under_a_resident_daemon() {
    // `search` is the hot read path: it MUST work while a daemon holds the
    // lock, not hit Busy. It routes to `search_memories` (a richer recall
    // pipeline, so ranking can differ from the direct all-label path — hence
    // it is excluded from the byte-parity set above), but the output SHAPE is
    // the same bare array of {node, score}, and the call must succeed.
    let Some(bin) = daemon_bin() else {
        eprintln!("skipping: target topodb-mcp not built");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("search.redb");
    run(&db, &["remember", "--content", "gamma about badgers", "--entity", "Badger"]);

    // Direct (no daemon): a bare array.
    let direct = run(&db, &["search", "badgers"]);
    assert!(direct.starts_with('['), "direct search should be a bare array: {direct}");

    // Routed (daemon resident): still a bare array, still finds the memory,
    // and crucially NOT a Busy error.
    let _daemon = start_daemon(&bin, &db);
    let routed = run(&db, &["search", "badgers"]);
    run(&db, &["daemon", "stop"]);
    assert!(
        routed.starts_with('['),
        "routed search must be a bare array (available, not Busy): {routed}"
    );
    let hits: serde_json::Value = serde_json::from_str(&routed).expect("routed search json");
    assert!(
        hits.as_array().is_some_and(|a| !a.is_empty()),
        "routed search should still recall the badgers memory: {routed}"
    );
}

#[test]
fn info_is_not_routed() {
    // `info` deliberately does not route (db_info is a lighter summary than
    // the direct full index_spec dump). A lone direct call prints the full
    // spec regardless of routing.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("info.redb");
    run(&db, &["remember", "--content", "delta note", "--entity", "Delta"]);
    let info = run(&db, &["info"]);
    let v: serde_json::Value = serde_json::from_str(&info).expect("info json");
    assert!(
        v.get("index_spec").is_some(),
        "direct info should include the full index_spec: {info}"
    );
}

/// Sanity: `daemon stop` on a db with no daemon reports not-live rather than
/// hanging, and a spawned daemon can then be stopped cleanly.
#[test]
fn daemon_stop_without_daemon_is_immediate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("nodaemon.redb");
    run(&db, &["create-memory", "--content", "seed"]);
    let out = run(&db, &["daemon", "stop"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("stop json");
    assert_eq!(
        v.get("stopped").and_then(|b| b.as_bool()),
        Some(false),
        "stop with no daemon should report stopped:false: {out}"
    );
}
