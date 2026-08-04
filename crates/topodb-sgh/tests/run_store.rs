use std::collections::BTreeMap;

use topodb::{Db, Op, PropValue, Scope, ScopeId, ScopeSet};
use topodb_sgh::schema::validate::validate;
use topodb_sgh::schema::Graph;
use topodb_sgh::store::run::{NodeState, RunStore};
use topodb_sgh::store::{
    SghError, EDGE_PRODUCED, EDGE_REVISION_OF, LABEL_NODE, LABEL_RUN, LABEL_RUN_INDEX,
};

fn store(db: &Db) -> RunStore {
    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();
    RunStore::create(db, "run-1", &v, 100).expect("create run")
}

#[test]
fn nodes_start_pending() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);
    assert_eq!(s.state("survey").unwrap(), NodeState::Pending);
    assert_eq!(s.state("build").unwrap(), NodeState::Pending);
}

#[test]
fn state_transitions_supersede_and_keep_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.set_state("survey", NodeState::Ready, 200).unwrap();
    s.set_state("survey", NodeState::Running, 300).unwrap();
    s.set_state("survey", NodeState::Succeeded, 400).unwrap();

    assert_eq!(s.state("survey").unwrap(), NodeState::Succeeded);

    // History is intact: as_of reads recover the past.
    assert_eq!(s.state_at("survey", 250).unwrap(), Some(NodeState::Ready));
    assert_eq!(s.state_at("survey", 350).unwrap(), Some(NodeState::Running));
}

#[test]
fn outputs_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    assert_eq!(s.output("survey").unwrap(), None);
    s.record_output("survey", r#"{"sites":[]}"#, 500).unwrap();
    assert_eq!(
        s.output("survey").unwrap().as_deref(),
        Some(r#"{"sites":[]}"#)
    );
}

#[test]
fn record_output_supersedes_prior_output() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_output("survey", r#"{"sites":[]}"#, 500).unwrap();
    s.record_output("survey", r#"{"sites":["a"]}"#, 600)
        .unwrap();

    assert_eq!(
        s.output("survey").unwrap().as_deref(),
        Some(r#"{"sites":["a"]}"#)
    );
}

/// A node's output write is atomic: output node + superseding PRODUCED edge
/// carry the same timestamp (one batch — no crash window between them).
///
/// `ChangeEvent` (see `crates/topodb/src/db.rs`) carries only `seq` + `op` —
/// no explicit batch/group id — so there is no `ops_since`-based same-batch
/// proof available; this uses the timestamp form the brief allows as
/// fallback: the closed (superseded) edge's `valid_to` and the freshly
/// opened edge's `valid_from` must be identical, which is only possible if
/// both writes (and the output node's creation) landed in the same
/// `submit_at` call.
#[test]
fn record_output_is_single_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_output("survey", "{}", 200).unwrap();
    assert_eq!(s.output("survey").unwrap().as_deref(), Some("{}"));

    s.record_output("survey", "{\"v\":2}", 201).unwrap();
    assert_eq!(
        s.output("survey").unwrap().as_deref(),
        Some("{\"v\":2}"),
        "second output visible"
    );

    // Inspect the PRODUCED edges directly via the debug dump (there is no
    // public unfiltered-by-as_of read, and this run only ever writes PRODUCED
    // edges for "survey"'s output): the first must be closed at 201 (the
    // second call's timestamp) and the second must open at 201 — both writes
    // for the second call share one batch.
    let all: Vec<_> = db
        .debug_dump_edges()
        .into_iter()
        .filter(|e| e.ty.as_str() == EDGE_PRODUCED)
        .collect();
    assert_eq!(all.len(), 2, "both output edges survive as history");

    let closed: Vec<_> = all.iter().filter(|e| e.valid_to.is_some()).collect();
    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].valid_to,
        Some(201),
        "prior PRODUCED edge closed at the second record_output's timestamp"
    );

    let open: Vec<_> = all.iter().filter(|e| e.valid_to.is_none()).collect();
    assert_eq!(open.len(), 1);
    assert_eq!(
        open[0].valid_from, 201,
        "new PRODUCED edge opens at the same timestamp — same batch as the close \
         and the output node's own creation"
    );
}

#[test]
fn attempts_accumulate() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_attempt("survey", "retry", "timeout", 600).unwrap();
    s.record_attempt("survey", "repair", "schema mismatch", 700)
        .unwrap();
    assert_eq!(s.attempts("survey").unwrap().len(), 2);
}

/// `attempts()` used to read with `as_of: None` (wall clock) instead of the
/// same deterministic sentinel `state()` and `output()` use. That only
/// worked because every caller in this crate stamps timestamps well behind
/// the real wall clock (the CLI uses `now = 1`); a caller stamping a run
/// with a future-dated timestamp got an empty attempt history back with no
/// error, silently. Use a `now_ms` far in the future to prove the read is no
/// longer anchored to real wall time.
#[test]
fn attempts_are_visible_even_when_the_run_is_stamped_far_in_the_future() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();

    // Comfortably past real wall-clock time (ms since epoch), but still well
    // under the crate's `as_of` sentinel of `i64::MAX - 1`.
    let future = 4_102_444_800_000i64; // year 2100
    let s = RunStore::create(&db, "run-future", &v, future).expect("create run");

    s.record_attempt("survey", "retry", "timeout", future + 100)
        .unwrap();
    s.record_attempt("survey", "repair", "schema mismatch", future + 200)
        .unwrap();

    let attempts = s.attempts("survey").unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "attempts recorded with future timestamps must still be visible"
    );
}

#[test]
fn a_run_starts_with_no_revision() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);
    assert_eq!(s.revision().unwrap(), None);
}

#[test]
fn revisions_round_trip_and_supersede() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.record_revision(
        "version: 1\ngoal: first\nnodes: []\n",
        "survey blocked",
        500,
    )
    .unwrap();
    let (yaml, reason) = s.revision().unwrap().expect("a revision exists");
    assert!(yaml.contains("first"));
    assert_eq!(reason, "survey blocked");

    s.record_revision(
        "version: 1\ngoal: second\nnodes: []\n",
        "build blocked",
        600,
    )
    .unwrap();
    let (yaml, reason) = s
        .revision()
        .unwrap()
        .expect("still exactly one open revision");
    assert!(yaml.contains("second"), "the latest proposal wins");
    assert_eq!(reason, "build blocked");

    // Durability: the first, superseded proposal must still exist as a
    // closed REVISION_OF edge with its payload intact — supersession closes
    // the old edge, it never deletes or overwrites it.
    let scope_id = match s.scope() {
        Scope::Id(id) => id,
        Scope::Shared => panic!("run scope must be Scope::Id"),
    };
    let scopes = ScopeSet::of(&[scope_id]);
    let all = db
        .edges_from(&scopes, s.run_node(), None, Some(EDGE_REVISION_OF), false)
        .unwrap();
    assert_eq!(
        all.len(),
        2,
        "both proposals survive as edges, none deleted"
    );

    let open: Vec<_> = all.iter().filter(|e| e.valid_to.is_none()).collect();
    assert_eq!(open.len(), 1, "exactly one open revision edge");
    let open_rec = db
        .node(&scopes, open[0].to)
        .expect("open revision node exists");
    match open_rec.props.get("yaml") {
        Some(PropValue::Str(s)) => assert!(
            s.contains("second"),
            "open edge points at the latest revision"
        ),
        other => panic!("expected yaml prop, got {other:?}"),
    }

    let closed: Vec<_> = all.iter().filter(|e| e.valid_to.is_some()).collect();
    assert_eq!(
        closed.len(),
        1,
        "exactly one closed (superseded) revision edge"
    );
    assert_eq!(
        closed[0].valid_to,
        Some(600),
        "superseded edge closed at the second call's timestamp"
    );

    let superseded_rec = db
        .node(&scopes, closed[0].to)
        .expect("superseded revision node exists");
    let superseded_yaml = match superseded_rec.props.get("yaml") {
        Some(PropValue::Str(s)) => s.clone(),
        other => panic!("expected yaml prop, got {other:?}"),
    };
    let superseded_reason = match superseded_rec.props.get("reason") {
        Some(PropValue::Str(s)) => s.clone(),
        other => panic!("expected reason prop, got {other:?}"),
    };
    assert!(
        superseded_yaml.contains("first"),
        "superseded revision's payload is still readable, not wiped"
    );
    assert_eq!(superseded_reason, "survey blocked");
}

#[test]
fn create_writes_a_shared_scope_index() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    let shared = ScopeSet::default().with_shared();
    let recs = db.nodes_by_label(&shared, LABEL_RUN_INDEX);
    assert_eq!(recs.len(), 1, "exactly one shared-scope index node");
    let rec = &recs[0];

    match rec.props.get("run_id") {
        Some(PropValue::Str(s)) => assert_eq!(s, "run-1"),
        other => panic!("expected run_id str prop, got {other:?}"),
    }
    match rec.props.get("status") {
        Some(PropValue::Str(s)) => assert_eq!(s, "running"),
        other => panic!("expected status str prop, got {other:?}"),
    }
    match rec.props.get("goal") {
        Some(PropValue::Str(g)) => assert_eq!(g, "port the search analyzer"),
        other => panic!("expected goal str prop, got {other:?}"),
    }
    match rec.props.get("created_at") {
        Some(PropValue::DateTime(t)) => assert_eq!(*t, 100),
        other => panic!("expected created_at datetime prop, got {other:?}"),
    }

    let scope_id = match s.scope() {
        Scope::Id(id) => id,
        Scope::Shared => panic!("run scope must be Scope::Id"),
    };
    match rec.props.get("scope_id") {
        Some(PropValue::Str(sid)) => {
            let parsed: ScopeId = sid.parse().expect("scope_id parses back to a ScopeId");
            assert_eq!(
                parsed, scope_id,
                "scope_id round-trips to the store's scope"
            );
        }
        other => panic!("expected scope_id str prop, got {other:?}"),
    }
}

#[test]
fn set_status_rewrites_the_index_prop() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    s.set_status("complete", 200).unwrap();

    let shared = ScopeSet::default().with_shared();
    let recs = db.nodes_by_label(&shared, LABEL_RUN_INDEX);
    assert_eq!(recs.len(), 1);
    let rec = &recs[0];

    match rec.props.get("status") {
        Some(PropValue::Str(status)) => assert_eq!(status, "complete"),
        other => panic!("expected status str prop, got {other:?}"),
    }
    match rec.props.get("created_at") {
        Some(PropValue::DateTime(t)) => assert_eq!(*t, 100, "created_at must not change"),
        other => panic!("expected created_at datetime prop, got {other:?}"),
    }
}

/// See `RunStore::set_status`'s doc comment: `high_water_ms` is the
/// cross-process defense against a resuming process whose wall clock reads
/// behind this run's own history (NTP correction, VM snapshot restore).
/// `create` stamps it at creation time and `set_status` re-stamps it on
/// every status change; both must be visible after a fresh `RunStore::open`
/// — the mark has to survive being read back in a new process, since that's
/// the only place it's ever consulted (`resume_cmd`).
#[test]
fn high_water_ms_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let _s = store(&db); // created at now_ms=100, see `store()` above

    let (reopened, _v) = RunStore::open(&db, "run-1").unwrap();
    assert_eq!(reopened.high_water_ms(), 100);

    reopened.set_status("complete", 500).unwrap();

    let (reopened_again, _v) = RunStore::open(&db, "run-1").unwrap();
    assert_eq!(reopened_again.high_water_ms(), 500);
}

#[test]
fn set_status_never_lowers_the_high_water_mark() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();

    let s = RunStore::create(&db, "run-hwm-test", &v, 1_000).expect("create run");

    s.set_status("running", 5_000).unwrap();
    assert_eq!(s.high_water_ms(), 5_000);
    // Clock steps back mid-run (NTP correction): the mark must hold.
    s.set_status("running", 3_000).unwrap();
    assert_eq!(
        s.high_water_ms(),
        5_000,
        "last_ms is a high-water mark; a stepped-back clock must not lower it"
    );
    // And it still advances.
    s.set_status("complete", 6_000).unwrap();
    assert_eq!(s.high_water_ms(), 6_000);
}

#[test]
fn graph_yaml_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    let yaml = s.graph_yaml().unwrap();
    let g = Graph::from_yaml(&yaml).unwrap();
    let v = validate(&g).unwrap();

    let orig = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let orig_v = validate(&orig).unwrap();

    assert_eq!(v.topo_order, orig_v.topo_order);
}

#[test]
fn index_is_the_only_shared_scope_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let _s = store(&db);

    let shared = ScopeSet::default().with_shared();
    assert!(
        db.nodes_by_label(&shared, LABEL_RUN).is_empty(),
        "SghRun nodes must stay in the run scope"
    );
    assert!(
        db.nodes_by_label(&shared, LABEL_NODE).is_empty(),
        "SghNode nodes must stay in the run scope"
    );
}

#[test]
fn open_round_trips_a_created_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    {
        let s = store(&db);
        assert_eq!(s.state("survey").unwrap(), NodeState::Pending);
    }
    // The original `RunStore` handle is dropped; reattach purely from the db.
    let (reopened, v) = RunStore::open(&db, "run-1").expect("run reopens");

    let orig = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let orig_v = validate(&orig).unwrap();
    assert_eq!(v.topo_order, orig_v.topo_order);

    assert_eq!(reopened.state("survey").unwrap(), NodeState::Pending);

    // Writes through the reopened handle work.
    reopened
        .set_state("survey", NodeState::Succeeded, 200)
        .unwrap();
    assert_eq!(reopened.state("survey").unwrap(), NodeState::Succeeded);
}

#[test]
fn open_sees_prior_progress() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    {
        let s = store(&db);
        s.set_state("survey", NodeState::Succeeded, 150).unwrap();
        s.record_output("survey", "{\"n\":1}", 150).unwrap();
        s.record_attempt("build", "retry", "boom", 160).unwrap();
    }

    let (reopened, _v) = RunStore::open(&db, "run-1").expect("run reopens");
    assert_eq!(reopened.state("survey").unwrap(), NodeState::Succeeded);
    assert_eq!(
        reopened.output("survey").unwrap().as_deref(),
        Some("{\"n\":1}")
    );
    let attempts = reopened.attempts("build").unwrap();
    assert!(
        attempts
            .iter()
            .any(|(rung, err)| rung == "retry" && err == "boom"),
        "expected (\"retry\", \"boom\") in {attempts:?}"
    );
}

#[test]
fn open_unknown_run_is_run_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let _s = store(&db);

    let err = match RunStore::open(&db, "nope") {
        Err(e) => e,
        Ok(_) => panic!("unknown run must error"),
    };
    match err {
        SghError::RunNotFound { run_id } => assert_eq!(run_id, "nope"),
        other => panic!("expected RunNotFound, got {other:?}"),
    }
}

#[test]
fn open_rejects_a_corrupt_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();
    let s = store(&db);

    let run_node = s.run_node();
    let mut props = BTreeMap::new();
    props.insert(
        "graph_yaml".to_string(),
        Some(PropValue::Str("version: 1\n".to_string())),
    );
    db.submit_at(
        vec![Op::SetNodeProps {
            id: run_node,
            props,
        }],
        300,
    )
    .unwrap();

    let err = match RunStore::open(&db, "run-1") {
        Err(e) => e,
        Ok(_) => panic!("corrupt graph must error"),
    };
    match err {
        SghError::CorruptRun { run_id, reason } => {
            assert_eq!(run_id, "run-1");
            assert!(!reason.is_empty(), "reason must name what went wrong");
        }
        other => panic!("expected CorruptRun, got {other:?}"),
    }
}

#[test]
fn duplicate_run_ids_are_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("t.redb")).unwrap();

    let g = Graph::from_yaml(include_str!("fixtures/simple.yaml")).unwrap();
    let v = validate(&g).unwrap();
    RunStore::create(&db, "dup-run", &v, 100).expect("first create");
    RunStore::create(&db, "dup-run", &v, 200).expect("second create, same run_id");

    let err = match RunStore::open(&db, "dup-run") {
        Err(e) => e,
        Ok(_) => panic!("duplicate index must be corrupt"),
    };
    match err {
        SghError::CorruptRun { run_id, .. } => assert_eq!(run_id, "dup-run"),
        other => panic!("expected CorruptRun, got {other:?}"),
    }
}
