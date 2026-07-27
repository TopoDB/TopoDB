use topodb::Db;
use topodb_sgh::events::{EventSink, JsonlSink, RunEvent, VecSink};
use topodb_sgh::executor::Executor;
use topodb_sgh::runner::mock::MockRunner;
use topodb_sgh::runner::NodeOutcome;
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::RunStore;

/// a -> b two-node chain, both plain agent nodes.
fn chain() -> topodb_sgh::schema::validate::Validated {
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, needs: [a], budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    validate(&g).unwrap()
}

#[test]
fn a_run_emits_the_expected_event_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = chain();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new();
    let sink = VecSink::new();

    let mut ex = Executor::new(store, v, &runner)
        .with_events(&sink)
        .with_run_id("r".into());
    let report = ex.run(10).unwrap();
    assert_eq!(report.succeeded, vec!["a".to_string(), "b".to_string()]);

    let events = sink.0.lock().unwrap();

    // ts must be non-decreasing across the whole sequence.
    let mut prev_ts = i64::MIN;
    for (ts, _) in events.iter() {
        assert!(*ts >= prev_ts, "timestamps must be non-decreasing");
        prev_ts = *ts;
    }

    let kinds: Vec<String> = events
        .iter()
        .map(|(_, ev)| match ev {
            RunEvent::RunStarted { .. } => "run_started".to_string(),
            RunEvent::NodeStarted { node_id } => format!("node_started:{node_id}"),
            RunEvent::AttemptFinished { node_id, .. } => format!("attempt_finished:{node_id}"),
            RunEvent::NodeSucceeded { node_id } => format!("node_succeeded:{node_id}"),
            RunEvent::NodeBlocked { node_id, .. } => format!("node_blocked:{node_id}"),
            RunEvent::NodeSkipped { node_id } => format!("node_skipped:{node_id}"),
            RunEvent::GateReached { node_id } => format!("gate_reached:{node_id}"),
            RunEvent::RunFinished { .. } => "run_finished".to_string(),
        })
        .collect();

    assert_eq!(
        kinds,
        vec![
            "run_started",
            "node_started:a",
            "node_succeeded:a",
            "node_started:b",
            "node_succeeded:b",
            "run_finished",
        ]
    );
}

#[test]
fn a_blocked_node_emits_attempt_and_block() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let v = chain();
    let store = RunStore::create(&db, "r", &v, 1).unwrap();
    let runner = MockRunner::new().script("a", vec![NodeOutcome::Failed { error: "x".into() }]);
    let sink = VecSink::new();

    let mut ex = Executor::new(store, v, &runner).with_events(&sink);
    let report = ex.run(10).unwrap();
    assert_eq!(report.blocked, vec!["a".to_string()]);

    let events = sink.0.lock().unwrap();
    let mut attempt_idx = None;
    let mut blocked_idx = None;
    for (i, (_, ev)) in events.iter().enumerate() {
        match ev {
            RunEvent::AttemptFinished { node_id, rung, .. }
                if node_id == "a" && rung == "block" =>
            {
                attempt_idx = Some(i);
            }
            RunEvent::NodeBlocked { node_id, reason } if node_id == "a" => {
                assert_eq!(reason.as_deref(), Some("x"));
                blocked_idx = Some(i);
            }
            _ => {}
        }
    }
    let attempt_idx = attempt_idx.expect("AttemptFinished{rung:block} for a");
    let blocked_idx = blocked_idx.expect("NodeBlocked for a");
    assert!(
        attempt_idx < blocked_idx,
        "AttemptFinished must precede NodeBlocked"
    );
}

/// Spot-check: the parallel scheduler (`with_max_inflight(2)`) must emit the
/// same skip/node events as the sequential path, just not necessarily in the
/// same interleaving. Uses two independent single-node chains (no shared
/// deps) so ordering between the two branches isn't pinned — only that each
/// branch's own NodeStarted -> NodeSucceeded pair appears, bracketed by
/// RunStarted/RunFinished, with non-decreasing timestamps throughout.
#[test]
fn parallel_mode_emits_node_events_too() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(
        "version: 1\ngoal: g\nnodes:\n\
         - {id: a, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n\
         - {id: b, kind: agent, prompt: p, budget: {retries: 0, repairs: 0}}\n",
    )
    .unwrap();
    let v = validate(&g).unwrap();
    let store = RunStore::create(&db, "rp", &v, 1).unwrap();
    let runner = MockRunner::new();
    let sink = VecSink::new();

    let mut ex = Executor::new(store, v, &runner)
        .with_events(&sink)
        .with_max_inflight(2);
    let report = ex.run(10).unwrap();
    assert_eq!(report.succeeded.len(), 2);

    let events = sink.0.lock().unwrap();

    let mut prev_ts = i64::MIN;
    for (ts, _) in events.iter() {
        assert!(*ts >= prev_ts, "timestamps must be non-decreasing");
        prev_ts = *ts;
    }

    assert!(matches!(
        events.first().unwrap().1,
        RunEvent::RunStarted { .. }
    ));
    assert!(matches!(
        events.last().unwrap().1,
        RunEvent::RunFinished { .. }
    ));

    for node in ["a", "b"] {
        let started = events
            .iter()
            .any(|(_, ev)| matches!(ev, RunEvent::NodeStarted { node_id } if node_id == node));
        let succeeded = events
            .iter()
            .any(|(_, ev)| matches!(ev, RunEvent::NodeSucceeded { node_id } if node_id == node));
        assert!(started, "NodeStarted missing for {node}");
        assert!(succeeded, "NodeSucceeded missing for {node}");
    }
}

#[test]
fn jsonl_sink_writes_versioned_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let sink = JsonlSink::create(&path).unwrap();

    sink.emit(
        1,
        &RunEvent::RunStarted {
            run_id: "r".into(),
            goal: "g".into(),
            agent_calls_bound: 2,
            command_runs_bound: 0,
        },
    );
    sink.emit(
        2,
        &RunEvent::NodeStarted {
            node_id: "a".into(),
        },
    );

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 2);

    let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v0["v"], 1);
    assert!(v0["ts"].is_number());
    assert_eq!(v0["event"], "run_started");

    let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(v1["v"], 1);
    assert!(v1["ts"].is_number());
    assert_eq!(v1["event"], "node_started");
}

/// Simulates a dead target by deleting the sink's parent directory out from
/// under it (unix only — Windows locks open files' containing directories
/// differently, and portably forcing an I/O error on an open file handle
/// isn't reliable). This test pins ONLY the following: emitting twice after
/// the parent directory is gone never panics, and the process keeps going.
/// It deliberately does NOT pin whether `disabled` ends up `true` — macOS in
/// particular may let writes to an already-open, now-unlinked file succeed
/// (the file stays valid via its inode until the last handle closes), so the
/// self-disable path may or may not actually trigger here.
#[cfg(unix)]
#[test]
fn jsonl_sink_survives_a_dead_target() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("events.jsonl");
    let sink = JsonlSink::create(&path).unwrap();

    std::fs::remove_dir_all(dir.path().join("sub")).unwrap();

    // Neither call may panic, regardless of whether the write itself
    // succeeds or fails on this platform.
    sink.emit(
        1,
        &RunEvent::NodeStarted {
            node_id: "a".into(),
        },
    );
    sink.emit(
        2,
        &RunEvent::NodeStarted {
            node_id: "b".into(),
        },
    );
}
