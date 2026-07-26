//! The shipped lifecycle graph (plugins/sgh/graphs/lifecycle.yaml): schema
//! validation everywhere; seeded-db executor runs (real ShellCommandRunner
//! for sweep/verify, mocked agent runner for judge) on POSIX hosts. Spec:
//! docs/superpowers/specs/2026-07-25-memory-lifecycle-design.md, Phase D.

use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;

fn graph_yaml() -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/sgh/graphs/lifecycle.yaml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The graph is a fixed artifact: 3 nodes, exact ids, judge opted into
/// mcp__topodb, verify downstream of the judge's schema-bearing claim
/// (the UncheckedClaim rule), both command nodes carrying output schemas.
#[test]
fn shipped_graph_validates_with_the_spec_shape() {
    let g = Graph::from_yaml(&graph_yaml()).expect("well-formed yaml");
    let v = validate(&g).expect("graph must pass sgh validate");
    let ids: Vec<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["sweep", "judge", "verify"]);
    let judge = &g.nodes[1];
    assert_eq!(
        judge.tools,
        vec!["topodb".to_string()],
        "judge acts via mcp__topodb"
    );
    assert!(judge.output.is_some(), "judge output is schema-checked");
    assert_eq!(
        g.nodes[2].needs,
        vec!["sweep".to_string(), "judge".to_string()]
    );
    let _ = v; // validation itself is the assertion
}

// The seeded-db runs exercise the real run strings under `sh -c` with jq —
// the plugin's runtime surface is POSIX/bash (mac/linux); Windows would only
// test path translation the plugin never ships through.
#[cfg(not(windows))]
mod seeded {
    use super::graph_yaml;
    use std::sync::Mutex;
    use std::time::Duration;
    use topodb_sgh::executor::Executor;
    use topodb_sgh::runner::command::ShellCommandRunner;
    use topodb_sgh::runner::mock::MockRunner;
    use topodb_sgh::runner::NodeOutcome;
    use topodb_sgh::schema::validate::validate;
    use topodb_sgh::schema::Graph;
    use topodb_sgh::store::run::RunStore;

    /// SGH_TOPODB/SGH_MEMORY_DB are process-global env; the two runs must
    /// not interleave their values.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Debug first: it is what `cargo test` just built; a stale release
    /// binary must not shadow it. Override for odd layouts.
    fn topodb_bin() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("SGH_TEST_TOPODB_BIN") {
            let p = std::path::PathBuf::from(p);
            return p.exists().then_some(p);
        }
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for profile in ["debug", "release"] {
            let p = ws
                .join("target")
                .join(profile)
                .join(format!("topodb{}", std::env::consts::EXE_SUFFIX));
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn cli(bin: &std::path::Path, db: &std::path::Path, args: &[&str]) -> serde_json::Value {
        let out = std::process::Command::new(bin)
            .arg("--db")
            .arg(db)
            .args(args)
            .output()
            .expect("spawn topodb");
        assert!(
            out.status.success(),
            "topodb {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("topodb prints json")
    }

    fn judge_output(acted: &[&str]) -> String {
        serde_json::json!({
            "decay": acted.iter().map(|id| serde_json::json!({
                "id": id, "verdict": "forget", "rationale": "scripted"
            })).collect::<Vec<_>>(),
            "duplicates": [],
            "acted_ids": acted,
        })
        .to_string()
    }

    struct RunResult {
        report: topodb_sgh::executor::RunReport,
        sweep_out: Option<String>,
        verify_out: Option<String>,
    }

    /// Seed two memories, optionally forget one via the real CLI (standing in
    /// for the judge's MCP action), then execute the shipped graph with the
    /// judge mocked to claim `acted` and the command nodes REAL.
    fn run_graph(
        acted_claim: &[&str],
        forget_first: Option<&str>,
    ) -> (tempfile::TempDir, Vec<String>, RunResult) {
        let bin = topodb_bin().expect("checked by caller");
        let dir = tempfile::tempdir().unwrap();
        let mem_db = dir.path().join("memory.redb");
        let a = cli(
            &bin,
            &mem_db,
            &[
                "remember",
                "--content",
                "alpha stale note",
                "--entity",
                "t",
                "--kind",
                "episodic",
            ],
        );
        let b = cli(
            &bin,
            &mem_db,
            &[
                "remember",
                "--content",
                "beta standing fact",
                "--entity",
                "t",
            ],
        );
        let ids = vec![
            a["memory_id"].as_str().unwrap().to_string(),
            b["memory_id"].as_str().unwrap().to_string(),
        ];
        if let Some(id) = forget_first {
            let id = if id == "A" { &ids[0] } else { id };
            cli(&bin, &mem_db, &["forget", id]);
        }
        let acted: Vec<&str> = acted_claim
            .iter()
            .map(|s| if *s == "A" { ids[0].as_str() } else { *s })
            .collect();

        let g = Graph::from_yaml(&graph_yaml()).unwrap();
        let v = validate(&g).unwrap();
        let run_db = topodb::Db::open(dir.path().join("run.redb")).unwrap();
        let store = RunStore::create(&run_db, "lifecycle-test", &v, 1).unwrap();
        let agent = MockRunner::new().script(
            "judge",
            vec![NodeOutcome::Succeeded {
                output: judge_output(&acted),
            }],
        );
        let commands = ShellCommandRunner::new(Duration::from_secs(60));

        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("SGH_TOPODB", &bin);
        std::env::set_var("SGH_MEMORY_DB", &mem_db);
        let mut ex = Executor::new(store, v, &agent).with_command_runner(&commands);
        let report = ex.run(2).unwrap();
        let sweep_out = ex.store_ref().output("sweep").unwrap();
        let verify_out = ex.store_ref().output("verify").unwrap();
        std::env::remove_var("SGH_TOPODB");
        std::env::remove_var("SGH_MEMORY_DB");
        (
            dir,
            ids,
            RunResult {
                report,
                sweep_out,
                verify_out,
            },
        )
    }

    /// Happy path: the sweep really runs the CLI (its stored output lists the
    /// live seeded memories), the judged action is reflected in the db, and
    /// verify emits the before/after report.
    #[test]
    fn lifecycle_run_verifies_judged_actions_on_a_seeded_db() {
        let Some(_) = topodb_bin() else {
            eprintln!("skipping: no topodb CLI binary (cargo build -p topodb-cli, or set SGH_TEST_TOPODB_BIN)");
            return;
        };
        let (_dir, ids, r) = run_graph(&["A"], Some("A"));
        assert_eq!(
            r.report.blocked,
            Vec::<String>::new(),
            "reasons: {:?}",
            r.report.blocked_reasons
        );
        assert_eq!(r.report.succeeded, vec!["sweep", "judge", "verify"]);

        let sweep: serde_json::Value =
            serde_json::from_str(&r.sweep_out.expect("sweep output stored")).unwrap();
        let listed: Vec<&str> = sweep
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(
            listed.contains(&ids[1].as_str()),
            "live memory surfaces in the sweep"
        );
        assert!(
            !listed.contains(&ids[0].as_str()),
            "tombstoned memory does not"
        );

        let report: serde_json::Value =
            serde_json::from_str(&r.verify_out.expect("verify output stored")).unwrap();
        assert_eq!(report["acted"], 1);
        assert_eq!(report["before_candidates"], 1);
        assert_eq!(report["after_candidates"], 1);
        assert_eq!(report["decay"][0]["verdict"], "forget");
    }

    /// The load-bearing property: a judge that CLAIMS an action it did not
    /// perform is caught — verify fails and the run blocks.
    #[test]
    fn verify_blocks_when_a_claimed_action_did_not_happen() {
        let Some(_) = topodb_bin() else {
            eprintln!("skipping: no topodb CLI binary (cargo build -p topodb-cli, or set SGH_TEST_TOPODB_BIN)");
            return;
        };
        let (_dir, _ids, r) = run_graph(&["A"], None); // claims A retired; A is live
        assert!(
            r.report.blocked.contains(&"verify".to_string()),
            "verify must block on a false claim; report: succeeded={:?} blocked={:?}",
            r.report.succeeded,
            r.report.blocked
        );
        let reason = r
            .report
            .blocked_reasons
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            reason.contains("still live"),
            "the failure names the lie: {reason}"
        );
    }
}
