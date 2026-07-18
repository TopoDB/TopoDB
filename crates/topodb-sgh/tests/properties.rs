use proptest::prelude::*;
use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::bound::worst_case;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::{Budget, Graph, Node, NodeKind};
use topodb_sgh::store::run::{NodeState, RunStore};

/// Generate a random DAG. Edges only ever point backwards in the node list,
/// so acyclicity holds by construction and every generated case is valid —
/// the same trick `determinism.rs` uses with modulo indices.
fn dag() -> impl Strategy<Value = Graph> {
    (1usize..8).prop_flat_map(|n| {
        let deps = prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3), n);
        let budgets = prop::collection::vec((0u32..3, 0u32..2), n);
        (Just(n), deps, budgets).prop_map(|(n, deps, budgets)| {
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
                        d.dedup();
                        d
                    };
                    Node {
                        id: format!("n{i}"),
                        kind: NodeKind::Agent,
                        needs,
                        prompt: Some("p".into()),
                        run: None,
                        output: None,
                        budget: Budget { retries: budgets[i].0, repairs: budgets[i].1 },
                    }
                })
                .collect();
            Graph { version: 1, goal: "g".into(), nodes }
        })
    })
}

/// Which nodes fail, drawn independently of the graph.
fn failure_mask() -> impl Strategy<Value = Vec<bool>> {
    prop::collection::vec(any::<bool>(), 8)
}

proptest! {
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

        let mut ex = Executor::new(store, v.clone(), &runner);
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
