use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::recovery::{contract_preserved, Repairer};
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::bound::worst_case;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::{Graph, Node};

fn one_node(retries: u32, repairs: u32) -> Graph {
    Graph::from_yaml(&format!(
        "version: 1\ngoal: g\nnodes:\n\
         - {{id: a, kind: agent, prompt: p, budget: {{retries: {retries}, repairs: {repairs}}}}}\n"
    ))
    .unwrap()
}

#[test]
fn retries_are_exhausted_before_blocking() {
    let g = one_node(2, 0);
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Failed {
            error: "boom".into(),
        }],
    );

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    assert_eq!(runner.call_count(), 3, "1 initial + 2 retries");
}

#[test]
fn a_retry_that_succeeds_ends_the_ladder() {
    let g = one_node(2, 0);
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![
            NodeOutcome::Failed {
                error: "flaky".into(),
            },
            NodeOutcome::Succeeded {
                output: "{}".into(),
            },
        ],
    );

    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();

    assert_eq!(report.succeeded, vec!["a".to_string()]);
    assert_eq!(runner.call_count(), 2, "stopped as soon as it succeeded");
}

#[test]
fn contract_preserving_repair_is_accepted() {
    let g = one_node(0, 1);
    let original: &Node = &g.nodes[0];
    let mut repaired = original.clone();
    repaired.prompt = Some("a better prompt".into());
    assert!(contract_preserved(original, &repaired));
}

#[test]
fn contract_breaking_repairs_are_rejected() {
    let g = one_node(0, 1);
    let original: &Node = &g.nodes[0];

    let mut renamed = original.clone();
    renamed.id = "b".into();
    assert!(
        !contract_preserved(original, &renamed),
        "id change is not a repair"
    );

    let mut redeped = original.clone();
    redeped.needs = vec!["z".into()];
    assert!(
        !contract_preserved(original, &redeped),
        "new dependency is not a repair"
    );

    let mut reschema = original.clone();
    reschema.output = Some(topodb_sgh::schema::OutputSpec {
        schema: serde_json::json!({"type": "object"}),
    });
    assert!(
        !contract_preserved(original, &reschema),
        "schema change is not a repair"
    );

    let mut rebudget = original.clone();
    rebudget.budget.retries = 99;
    assert!(
        !contract_preserved(original, &rebudget),
        "budget change is not a repair"
    );

    let mut rekinded = original.clone();
    rekinded.kind = topodb_sgh::schema::NodeKind::Command;
    assert!(
        !contract_preserved(original, &rekinded),
        "kind change is not a repair"
    );

    // `run` is the command-node analogue of `prompt`. It is inert today
    // because commands always get a repair budget of 0 (never reach the
    // REPAIR rung), but `contract_preserved` itself must not treat an
    // arbitrary rewrite of the shell command as contract-preserving — the
    // instant command repair is implemented, this is the check that keeps a
    // repairer from rewriting `run` freely.
    let mut rerun = original.clone();
    rerun.run = Some("rm -rf /".into());
    assert!(
        !contract_preserved(original, &rerun),
        "run change is not a repair"
    );
}

#[test]
fn repair_is_attempted_after_retries_and_records_an_attempt() {
    let g = one_node(1, 1);
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Failed {
            error: "boom".into(),
        }],
    );

    struct AlwaysRepairs;
    impl Repairer for AlwaysRepairs {
        fn repair(&self, node: &Node, _error: &str) -> Option<Node> {
            let mut n = node.clone();
            n.prompt = Some("revised".into());
            Some(n)
        }
    }

    let mut ex = Executor::new(store, v, &runner).with_repairer(&AlwaysRepairs);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    // 1 initial + 1 retry + 1 repaired execution
    assert_eq!(runner.call_count(), 3);

    let attempts = ex.store_ref().attempts("a").unwrap();
    assert!(attempts.iter().any(|(rung, _)| rung == "retry"));
    assert!(attempts.iter().any(|(rung, _)| rung == "repair"));
}

/// End-to-end: a `Repairer` that returns a contract-breaking `Node` must
/// have its repair refused by the executor, not merely by the pure
/// `contract_preserved` function in isolation. This is the wiring in
/// `execute_node` (around the `Rung::Repair` match arm) that a future
/// refactor could accidentally bypass — e.g. by forgetting to check
/// `contract_preserved` before accepting `repairer.repair()`'s output, or
/// by checking it but still looping back to re-run the node regardless of
/// the result.
#[test]
fn contract_breaking_repair_is_rejected_and_node_blocks_without_reexecuting() {
    let g = one_node(0, 1);
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&db, "r", &v, 1).unwrap();
    // The node fails every time it is invoked; if the rejected repair were
    // ever executed anyway, this would surface as an extra call.
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Failed {
            error: "boom".into(),
        }],
    );

    struct ContractBreakingRepairer;
    impl Repairer for ContractBreakingRepairer {
        fn repair(&self, node: &Node, _error: &str) -> Option<Node> {
            // Widens the contract by adding a dependency that was never in
            // the frozen graph — exactly the "silent replan" the invariant
            // exists to forbid.
            let mut n = node.clone();
            n.needs.push("ghost".into());
            Some(n)
        }
    }

    let mut ex = Executor::new(store, v, &runner).with_repairer(&ContractBreakingRepairer);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    // retries: 0, so the very first failure already lands on the REPAIR
    // rung. The repairer is consulted exactly once, its (contract-breaking)
    // output is rejected, and the ladder blocks immediately without looping
    // back to the top to invoke the runner again. So the runner should have
    // been called exactly once: the initial attempt, and nothing after.
    assert_eq!(
        runner.call_count(),
        1,
        "a rejected repair must not trigger another execution of the node"
    );
}

/// Positive counterpart to the test above: with the same shape of graph and
/// failure, a `Repairer` whose output *does* preserve the contract (only
/// `prompt` differs) is accepted and the node is re-executed and allowed to
/// succeed. Paired with the rejection test, this proves the branch in
/// `execute_node` actually discriminates on `contract_preserved` rather than
/// always blocking on REPAIR (a test that only showed blocking would pass
/// even if repairs were unconditionally refused).
#[test]
fn contract_preserving_repair_is_accepted_end_to_end_and_node_succeeds() {
    let g = one_node(0, 1);
    let v = validate(&g).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script(
        "a",
        vec![
            NodeOutcome::Failed {
                error: "boom".into(),
            },
            NodeOutcome::Succeeded {
                output: "{}".into(),
            },
        ],
    );

    struct PromptOnlyRepairer;
    impl Repairer for PromptOnlyRepairer {
        fn repair(&self, node: &Node, _error: &str) -> Option<Node> {
            let mut n = node.clone();
            n.prompt = Some("revised prompt".into());
            Some(n)
        }
    }

    let mut ex = Executor::new(store, v, &runner).with_repairer(&PromptOnlyRepairer);
    let report = ex.run(10).unwrap();

    assert_eq!(report.succeeded, vec!["a".to_string()]);
    // 1 initial failure + 1 repaired execution that succeeds.
    assert_eq!(runner.call_count(), 2);
}

/// `bound.rs` budgets an agent node `1 + retries + 2*repairs` model calls,
/// explicitly including one call to consult the recovery model per repair.
/// Before this fix, `execute_node` never incremented `model_calls` for the
/// consultation itself (only for node re-executions), so `RunReport.
/// model_calls` and `Bound.agent_calls` metered different things. Drive a
/// node through every rung the bound accounts for (retries, then a repair
/// consultation, then a repaired re-execution that still fails and blocks)
/// and check the two totals agree exactly.
#[test]
fn model_calls_counts_the_repair_consultation_and_matches_the_bound() {
    let g = one_node(1, 1); // bound: 1 + 1 + 2*1 = 4
    let v = validate(&g).unwrap();
    let bound = worst_case(&v);
    assert_eq!(bound.agent_calls, 4);

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = topodb_sgh::store::run::RunStore::create(&db, "r", &v, 1).unwrap();
    // Fails every time: 1 initial + 1 retry + 1 repaired re-execution, all
    // failures, so the ladder exhausts every rung the bound budgets for.
    let runner = MockRunner::new().script(
        "a",
        vec![NodeOutcome::Failed {
            error: "boom".into(),
        }],
    );

    struct PromptOnlyRepairer;
    impl Repairer for PromptOnlyRepairer {
        fn repair(&self, node: &Node, _error: &str) -> Option<Node> {
            let mut n = node.clone();
            n.prompt = Some("revised".into());
            Some(n)
        }
    }

    let mut ex = Executor::new(store, v, &runner).with_repairer(&PromptOnlyRepairer);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["a".to_string()]);
    // 1 initial execution + 1 retry execution + 1 repair consultation + 1
    // repaired execution = 4, exactly the bound — not 3 (which is what the
    // unfixed executor reported, since it only counted executions and
    // dropped the consultation).
    assert_eq!(
        report.model_calls, 4,
        "model_calls must count the repair consultation, matching the published bound"
    );
    assert!(
        report.model_calls <= bound.agent_calls,
        "must never exceed the bound"
    );
}
