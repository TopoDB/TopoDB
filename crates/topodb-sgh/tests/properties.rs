use proptest::prelude::*;
use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::recovery::Repairer;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::bound::worst_case;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::{Budget, Graph, Node, NodeKind};
use topodb_sgh::store::run::{NodeState, RunStore};

/// A contract-preserving repairer that changes only `prompt`, wired into the
/// termination property so the REPAIR rung (and the `2*repairs` term of the
/// bound) actually gets exercised. With `NoopRepairer` (the executor's
/// default), every repair is declined and the ladder always falls straight
/// from RETRY to BLOCK, so `repairs_left` never does anything and a
/// regression that let the executor over-run its repair budget would pass
/// undetected.
struct PromptOnlyRepairer;
impl Repairer for PromptOnlyRepairer {
    fn repair(&self, node: &Node, error: &str) -> Option<Node> {
        let mut n = node.clone();
        n.prompt = Some(format!("retry after: {error}"));
        Some(n)
    }
}

/// Generate a random DAG. Edges only ever point backwards in the node list,
/// so acyclicity holds by construction and every generated case is valid —
/// the same trick `determinism.rs` uses with modulo indices.
///
/// Node kind is `Agent` or `Gate`, never `Command`: after Fix 1,
/// `Executor::run` refuses any graph containing a command node outright, so
/// generating one would make the run error rather than exercise these
/// properties. Command refusal has its own targeted tests in
/// `tests/executor.rs`; that's where it belongs, not diluted into a property
/// that would just short-circuit on it.
///
/// `needs` is deliberately **not** deduped: duplicated entries are exactly
/// the input that used to break the validator (see Fix 2 in
/// `schema/validate.rs`) — a case the generator was previously sanitizing
/// away before it ever reached the validator.
fn dag() -> impl Strategy<Value = Graph> {
    (1usize..8).prop_flat_map(|n| {
        let deps = prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3), n);
        let budgets = prop::collection::vec((0u32..3, 0u32..2), n);
        let is_gate = prop::collection::vec(any::<bool>(), n);
        (Just(n), deps, budgets, is_gate).prop_map(|(n, deps, budgets, is_gate)| {
            let nodes = (0..n)
                .map(|i| {
                    let needs = if i == 0 {
                        Vec::new()
                    } else {
                        let mut d: Vec<String> = deps[i]
                            .iter()
                            .map(|raw| format!("n{}", (*raw as usize) % i))
                            .collect();
                        d.sort();
                        d
                    };
                    let gate = is_gate[i];
                    Node {
                        id: format!("n{i}"),
                        kind: if gate {
                            NodeKind::Gate
                        } else {
                            NodeKind::Agent
                        },
                        needs,
                        prompt: if gate { None } else { Some("p".into()) },
                        run: None,
                        output: None,
                        budget: Budget {
                            retries: budgets[i].0,
                            repairs: budgets[i].1,
                        },
                    }
                })
                .collect();
            Graph {
                version: 1,
                goal: "g".into(),
                nodes,
            }
        })
    })
}

/// Which nodes fail, drawn independently of the graph.
fn failure_mask() -> impl Strategy<Value = Vec<bool>> {
    prop::collection::vec(any::<bool>(), 8)
}

/// Configure the proptest case count from the environment.
/// Routine runs use 32 cases (fast, suitable for every commit).
/// CI and releases should use SGH_PROPTEST_CASES=256+ for thorough coverage.
/// This is a speed/coverage tradeoff, not a weakening of any property.
fn cases() -> u32 {
    std::env::var("SGH_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    /// TERMINATION: every run reaches a terminal state, and never exceeds the
    /// bound computed from the graph alone before the run started.
    #[test]
    fn run_terminates_within_the_computed_bound(g in dag(), mask in failure_mask()) {
        let v = validate(&g).expect("generated graphs are valid by construction");
        let bound = worst_case(&v);

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let store = RunStore::create(&db, "r", &v, 1).unwrap();

        let mut runner = MockRunner::new();
        for (i, node) in v.graph.nodes.iter().enumerate() {
            if mask[i % mask.len()] {
                runner = runner.script(
                    &node.id,
                    vec![NodeOutcome::Failed { error: "injected".into() }],
                );
            }
        }

        // A contract-preserving repairer, not the executor's default
        // `NoopRepairer`, so failing nodes actually climb to the REPAIR rung
        // and the `2*repairs` term of the bound is exercised rather than
        // dead weight.
        let mut ex = Executor::new(store, v.clone(), &runner).with_repairer(&PromptOnlyRepairer);
        let report = ex.run(10).unwrap();

        // Terminal: every node ended in exactly one terminal state.
        let total = report.succeeded.len() + report.blocked.len() + report.skipped.len();
        prop_assert_eq!(total, v.graph.nodes.len(), "every node reached a terminal state");

        // Bounded: never more model calls than promised at approval time.
        prop_assert!(
            report.model_calls <= bound.agent_calls,
            "run made {} model calls, bound promised at most {}",
            report.model_calls,
            bound.agent_calls
        );
    }

    /// SOUNDNESS: a node only ever runs once all its dependencies succeeded.
    /// Contrapositive: any node that ran and has a dependency that did not
    /// succeed is a violation.
    #[test]
    fn no_node_runs_before_its_dependencies_succeed(g in dag(), mask in failure_mask()) {
        let v = validate(&g).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let store = RunStore::create(&db, "r", &v, 1).unwrap();

        let mut runner = MockRunner::new();
        for (i, node) in v.graph.nodes.iter().enumerate() {
            if mask[i % mask.len()] {
                runner = runner.script(&node.id, vec![NodeOutcome::Failed { error: "x".into() }]);
            }
        }

        let mut ex = Executor::new(store, v.clone(), &runner);
        let report = ex.run(10).unwrap();

        let ran: std::collections::HashSet<&String> =
            report.succeeded.iter().chain(report.blocked.iter()).collect();

        for node in &v.graph.nodes {
            if ran.contains(&node.id) {
                for dep in &node.needs {
                    prop_assert!(
                        report.succeeded.contains(dep),
                        "node {} ran but dependency {} did not succeed",
                        node.id,
                        dep
                    );
                }
            }
        }
    }

    /// DETERMINISM: identical mock outputs produce an identical schedule.
    #[test]
    fn identical_outputs_produce_an_identical_schedule(g in dag()) {
        let v = validate(&g).unwrap();

        let mut schedules = Vec::new();
        for _ in 0..2 {
            let dir = tempfile::tempdir().unwrap();
            let db = Db::open(dir.path().join("t.redb")).unwrap();
            let store = RunStore::create(&db, "r", &v, 1).unwrap();
            let runner = MockRunner::new();
            let mut ex = Executor::new(store, v.clone(), &runner);
            ex.run(10).unwrap();
            schedules.push(runner.calls());
        }

        prop_assert_eq!(&schedules[0], &schedules[1]);
    }

    /// IMMUTABILITY: state history is never overwritten. After a run, every
    /// node that reached a terminal state still has its earlier PENDING
    /// state (set at run creation, t=1) recoverable via an `as_of` read at
    /// tick 10 — before the run's own state transitions (which all occur
    /// at ticks > 10) have taken effect.
    ///
    /// Strengthened beyond the brief: the brief's version only checks
    /// `topo_order[0]`, which exercises just one node out of the whole
    /// generated DAG — most of what proptest generates (multi-node graphs,
    /// varied dependency shapes) is unused by that assertion. Checking every
    /// node instead means a regression that corrupts history for a
    /// dependent node, a leaf, or any node beyond the first in topological
    /// order would still be caught; the weaker version would pass even if
    /// only node 0's history happened to be preserved correctly.
    #[test]
    fn state_history_is_preserved(g in dag()) {
        let v = validate(&g).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let store = RunStore::create(&db, "r", &v, 1).unwrap();
        let runner = MockRunner::new();

        let all_ids: Vec<String> = v.topo_order.clone();
        let mut ex = Executor::new(store, v.clone(), &runner);
        ex.run(10).unwrap();

        // Reconstructed via the store, which is the only as_of-capable path.
        // At tick 10 every node was still PENDING (set at run creation, t=1);
        // none of the run's own transitions happen until tick > 10.
        for id in &all_ids {
            let past = ex.store_ref().state_at(id, 10).unwrap();
            prop_assert_eq!(past, Some(NodeState::Pending), "node {} lost its historical PENDING state", id);
        }
    }
}
