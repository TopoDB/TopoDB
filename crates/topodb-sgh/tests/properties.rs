use proptest::prelude::*;
use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::recovery::Repairer;
use topodb_sgh::runner::command::MockCommandRunner;
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
/// Node kind is `Agent`, `Gate`, or `Command` (roughly a third of nodes land
/// on each of the latter two). `Executor::run` refuses any graph containing
/// a command node unless a `CommandRunner` is configured, so every property
/// below that drives `dag()` through an `Executor` wires one up via
/// `.with_command_runner(..)` even when it doesn't otherwise care about
/// commands — otherwise those properties would start failing the moment the
/// generator produces a command node, for a reason unrelated to what they
/// assert. Command *refusal itself* has its own targeted tests in
/// `tests/executor.rs`.
///
/// `needs` is deliberately **not** deduped: duplicated entries are exactly
/// the input that used to break the validator (see Fix 2 in
/// `schema/validate.rs`) — a case the generator was previously sanitizing
/// away before it ever reached the validator.
fn dag() -> impl Strategy<Value = Graph> {
    (1usize..8).prop_flat_map(|n| {
        let deps = prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3), n);
        let budgets = prop::collection::vec((0u32..3, 0u32..2), n);
        // 0 => Agent, 1 => Gate, 2 => Command: roughly a third each.
        let kind_pick = prop::collection::vec(0u8..3, n);
        (Just(n), deps, budgets, kind_pick).prop_map(|(n, deps, budgets, kind_pick)| {
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
                    let kind = match kind_pick[i] {
                        0 => NodeKind::Agent,
                        1 => NodeKind::Gate,
                        _ => NodeKind::Command,
                    };
                    let (prompt, run) = match kind {
                        NodeKind::Agent => (Some("p".into()), None),
                        NodeKind::Gate => (None, None),
                        NodeKind::Command => (None, Some("true".into())),
                    };
                    Node {
                        id: format!("n{i}"),
                        kind,
                        needs,
                        prompt,
                        run,
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
        let mut commands = MockCommandRunner::new();
        for (i, node) in v.graph.nodes.iter().enumerate() {
            if mask[i % mask.len()] {
                match node.kind {
                    NodeKind::Command => {
                        commands = commands.script(
                            &node.id,
                            vec![NodeOutcome::Failed { error: "injected".into() }],
                        );
                    }
                    _ => {
                        runner = runner.script(
                            &node.id,
                            vec![NodeOutcome::Failed { error: "injected".into() }],
                        );
                    }
                }
            }
        }

        // A contract-preserving repairer, not the executor's default
        // `NoopRepairer`, so failing nodes actually climb to the REPAIR rung
        // and the `2*repairs` term of the bound is exercised rather than
        // dead weight. A command runner is wired up too, since `dag()` now
        // emits `Command` nodes and the executor refuses those outright
        // without one.
        let mut ex = Executor::new(store, v.clone(), &runner)
            .with_repairer(&PromptOnlyRepairer)
            .with_command_runner(&commands);
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
        let mut commands = MockCommandRunner::new();
        for (i, node) in v.graph.nodes.iter().enumerate() {
            if mask[i % mask.len()] {
                match node.kind {
                    NodeKind::Command => {
                        commands =
                            commands.script(&node.id, vec![NodeOutcome::Failed { error: "x".into() }]);
                    }
                    _ => {
                        runner =
                            runner.script(&node.id, vec![NodeOutcome::Failed { error: "x".into() }]);
                    }
                }
            }
        }

        let mut ex = Executor::new(store, v.clone(), &runner).with_command_runner(&commands);
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
            let commands = MockCommandRunner::new();
            let mut ex = Executor::new(store, v.clone(), &runner).with_command_runner(&commands);
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
        let commands = MockCommandRunner::new();

        let all_ids: Vec<String> = v.topo_order.clone();
        let mut ex = Executor::new(store, v.clone(), &runner).with_command_runner(&commands);
        ex.run(10).unwrap();

        // Reconstructed via the store, which is the only as_of-capable path.
        // At tick 10 every node was still PENDING (set at run creation, t=1);
        // none of the run's own transitions happen until tick > 10.
        for id in &all_ids {
            let past = ex.store_ref().state_at(id, 10).unwrap();
            prop_assert_eq!(past, Some(NodeState::Pending), "node {} lost its historical PENDING state", id);
        }
    }

    /// COMMAND BOUND: command executions never exceed the command_runs the
    /// graph budgeted, and command nodes never consume model calls.
    #[test]
    fn command_runs_stay_within_the_computed_bound(g in dag(), mask in failure_mask()) {
        let v = validate(&g).expect("generated graphs are valid by construction");
        let bound = worst_case(&v);

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        let store = RunStore::create(&db, "r", &v, 1).unwrap();

        let mut agents = MockRunner::new();
        let mut commands = MockCommandRunner::new();
        for (i, node) in v.graph.nodes.iter().enumerate() {
            if mask[i % mask.len()] {
                match node.kind {
                    NodeKind::Command => {
                        commands = commands.script(
                            &node.id,
                            vec![NodeOutcome::Failed { error: "injected".into() }],
                        );
                    }
                    _ => {
                        agents = agents.script(
                            &node.id,
                            vec![NodeOutcome::Failed { error: "injected".into() }],
                        );
                    }
                }
            }
        }

        let mut ex = Executor::new(store, v.clone(), &agents).with_command_runner(&commands);
        let report = ex.run(10).unwrap();

        prop_assert!(
            report.command_runs <= bound.command_runs,
            "ran {} command(s), bound promised at most {}",
            report.command_runs,
            bound.command_runs
        );
        prop_assert!(report.model_calls <= bound.agent_calls);

        let total = report.succeeded.len() + report.blocked.len() + report.skipped.len();
        prop_assert_eq!(total, v.graph.nodes.len(), "every node reached a terminal state");
    }
}

/// PLANNER BOUND: the planner's retry loop is bounded by max_attempts, no
/// matter how many times the backend produces an invalid graph.
#[test]
fn planner_retry_loop_is_bounded() {
    use std::sync::Mutex;
    use topodb_sgh::planner::claude::{ClaudePlanner, PlanBackend};
    use topodb_sgh::planner::{PlanRequest, Planner, PlannerError};

    struct AlwaysInvalid {
        calls: Mutex<u32>,
    }
    impl PlanBackend for AlwaysInvalid {
        fn complete(&self, _prompt: &str) -> Result<String, PlannerError> {
            *self.calls.lock().unwrap() += 1;
            Ok("version: 1\ngoal: g\nnodes:\n  - {id: a, kind: agent, needs: [ghost], budget: {retries: 0, repairs: 0}}\n".into())
        }
    }

    for max in [1u32, 2, 5] {
        let backend = std::sync::Arc::new(AlwaysInvalid { calls: Mutex::new(0) });
        let p = ClaudePlanner::with_backend(Box::new(backend.clone()), max);
        let err = p
            .plan(&PlanRequest { goal: "g".into(), context: None })
            .expect_err("never validates");
        assert!(matches!(err, PlannerError::Exhausted { .. }));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            max,
            "exactly max_attempts backend calls, never more"
        );
    }
}
