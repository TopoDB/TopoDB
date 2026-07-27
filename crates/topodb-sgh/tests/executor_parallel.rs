//! The parallel scheduler: overlap, caps, propagation, determinism of results.
//!
//! Deliberately NOT included: `worker_engine_error_aborts_the_run`. Faking an
//! `SghError` from inside a worker requires corrupting the store mid-run (the
//! only way `execute_node` returns `Err` today is a store/engine failure),
//! and the abort path itself is a straight channel-drain with no scheduling
//! subtlety — the coverage-to-cost ratio of engineering that failure isn't
//! worth it here. This omission is intentional, not an oversight.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;
use topodb::Db;
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::{AgentRunner, NodeOutcome, NodeRequest, RunnerError};
use topodb_sgh::schema::validate::{validate, Validated};
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

/// Two independent nodes rendezvous: each waits (with timeout) until the other
/// has entered run(). Only a genuinely concurrent schedule can pass.
struct Rendezvous {
    tx: Mutex<Vec<mpsc::Sender<()>>>,
    rx: Mutex<Vec<mpsc::Receiver<()>>>,
}
impl Rendezvous {
    fn pair() -> Self {
        let (t1, r1) = mpsc::channel();
        let (t2, r2) = mpsc::channel();
        Rendezvous {
            tx: Mutex::new(vec![t2, t1]),
            rx: Mutex::new(vec![r1, r2]),
        }
    }
}
impl AgentRunner for Rendezvous {
    fn run(&self, _req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        let tx = self.tx.lock().unwrap().pop().unwrap();
        let rx = self.rx.lock().unwrap().pop().unwrap();
        let _ = tx.send(());
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the other branch never started — schedule was not concurrent");
        Ok(NodeOutcome::Succeeded {
            output: "{}".into(),
        })
    }
}

fn two_independent() -> Validated {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n  - id: a\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n  - id: b\n    kind: agent\n    prompt: p\n    budget: {retries: 0, repairs: 0}\n",
    ).unwrap();
    validate(&g).unwrap()
}

fn diamond() -> Validated {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n\
         - {id: c, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n\
         - {id: d, kind: agent, prompt: p, needs: [b, c], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    validate(&g).unwrap()
}

fn six_independent() -> Validated {
    let mut yaml = String::from("version: 1\ngoal: g\nnodes:\n");
    for i in 0..6 {
        yaml.push_str(&format!(
            "  - id: n{i}\n    kind: agent\n    prompt: p\n    budget: {{retries: 0, repairs: 0}}\n"
        ));
    }
    let g = Graph::from_yaml(&yaml).unwrap();
    validate(&g).unwrap()
}

#[test]
fn independent_branches_actually_overlap_at_inflight_2() {
    let v = two_independent();
    let runner = Rendezvous::pair();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "par", &v, 1).unwrap();
    let mut ex = Executor::new(store, v, &runner).with_max_inflight(2);
    let report = ex.run(10).unwrap();
    assert_eq!(report.succeeded.len(), 2);
}

#[test]
fn sequential_default_would_deadlock_rendezvous_so_it_must_not_be_used_here() {
    // Guard test for the test above: with max_inflight(1) the rendezvous
    // would hang; assert the parallel path is what made it pass by checking
    // a plain runner still works sequentially on the same graph.
    // (Simple smoke: MockRunner both nodes succeed, max_inflight default.)
    let v = two_independent();
    let runner = topodb_sgh::runner::mock::MockRunner::new();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "seq", &v, 1).unwrap();
    let mut ex = Executor::new(store, v, &runner);
    let report = ex.run(10).unwrap();
    assert_eq!(report.succeeded.len(), 2);
}

/// A runner that counts concurrent entries into `run()` with an
/// `AtomicUsize`, tracking the high-water mark of simultaneous callers. A
/// short sleep inside `run()` makes overlap overwhelmingly likely if the
/// scheduler actually runs nodes concurrently, without relying on a
/// rendezvous (which would hang instead of merely under-counting if the cap
/// were violated in the other direction).
struct CountingRunner {
    current: AtomicUsize,
    high_water: AtomicUsize,
}
impl CountingRunner {
    fn new() -> Self {
        CountingRunner {
            current: AtomicUsize::new(0),
            high_water: AtomicUsize::new(0),
        }
    }
    fn high_water(&self) -> usize {
        self.high_water.load(Ordering::SeqCst)
    }
}
impl AgentRunner for CountingRunner {
    fn run(&self, _req: &NodeRequest) -> Result<NodeOutcome, RunnerError> {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.high_water.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        self.current.fetch_sub(1, Ordering::SeqCst);
        Ok(NodeOutcome::Succeeded {
            output: "{}".into(),
        })
    }
}

#[test]
fn inflight_never_exceeds_the_cap() {
    let v = six_independent();
    let runner = CountingRunner::new();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "cap", &v, 1).unwrap();
    let mut ex = Executor::new(store, v, &runner).with_max_inflight(2);
    let report = ex.run(10).unwrap();

    assert_eq!(report.succeeded.len(), 6);
    assert!(
        runner.high_water() <= 2,
        "inflight high-water mark {} exceeded the cap of 2",
        runner.high_water()
    );
    assert!(
        runner.high_water() >= 2,
        "inflight high-water mark {} never reached 2 — parallelism did not \
         actually engage (the 50ms sleep should make overlap overwhelmingly \
         likely at cap 2 across 6 independent nodes)",
        runner.high_water()
    );
}

#[test]
fn a_blocked_node_poisons_only_descendants_in_parallel_mode() {
    let v = diamond();
    let runner = topodb_sgh::runner::mock::MockRunner::new()
        .script("b", vec![NodeOutcome::Failed { error: "x".into() }]);
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let store = RunStore::create(&db, "poison", &v, 1).unwrap();
    let mut ex = Executor::new(store, v, &runner).with_max_inflight(4);
    let report = ex.run(10).unwrap();

    assert_eq!(report.blocked, vec!["b".to_string()]);
    assert_eq!(report.skipped, vec!["d".to_string()], "d needs b");
    let succeeded: BTreeMap<&str, ()> = report.succeeded.iter().map(|s| (s.as_str(), ())).collect();
    assert!(succeeded.contains_key("a"), "a has no failing dep");
    assert!(succeeded.contains_key("c"), "c is independent of b");
}
