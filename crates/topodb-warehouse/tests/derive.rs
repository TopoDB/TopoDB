use topodb::{Db, NodeId, Op, PropValue, Scope, TimeAxis};
use topodb_warehouse::event::*;
use topodb_warehouse::{
    derive, derived_by_stamp, Warehouse, WarehouseConfig, ARTIFACT_LABEL, CHUNK_LABEL,
    EVIDENCE_EDGE, HAS_CHUNK_EDGE,
};

const SCOPE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn art(id: &str, ts: i64, locator: &str, content: &str, tool: &str, kind: ArtifactType) -> Event {
    Event {
        id: id.into(),
        ts,
        host: String::new(),
        kind: Kind::Artifact,
        v: EVENT_VERSION,
        source: Some(Source {
            harness: "claude-code".into(),
            session: "s1".into(),
            scope: SCOPE.into(),
            turn: None,
            tool: Some(tool.into()),
            cwd: None,
            agent: None,
        }),
        artifact: Some(Artifact {
            kind,
            locator: locator.into(),
            bytes: 0,
            hash: None,
            redacted: false,
            redactions: vec![],
            redact_v: None,
            content: Some(content.into()),
            blob: None,
        }),
        op: None,
        marker: None,
    }
}
fn marker(id: &str, ts: i64, kind: MarkerType, node_ids: Vec<String>) -> Event {
    Event {
        id: id.into(),
        ts,
        host: String::new(),
        kind: Kind::Marker,
        v: EVENT_VERSION,
        source: None,
        artifact: None,
        op: None,
        marker: Some(Marker {
            kind,
            harness: "claude-code".into(),
            session: "s1".into(),
            scope: SCOPE.into(),
            node_ids,
        }),
    }
}
fn spool(wh: &Warehouse, name: &str, evs: &[Event]) {
    let s: String = evs.iter().map(encode_line).collect();
    std::fs::write(wh.layout.spool.join(name), s).unwrap();
}
fn scope() -> Scope {
    topodb_json::resolve_scope(Some(SCOPE), Scope::Shared).unwrap()
}
fn memory(db: &Db, text: &str) -> NodeId {
    let id = NodeId::new();
    let mut props = topodb::Props::new();
    props.insert("content".into(), PropValue::Str(text.into()));
    db.submit(vec![Op::CreateNode {
        id,
        scope: scope(),
        label: "Memory".into(),
        props,
    }])
    .unwrap();
    id
}
fn setup() -> (tempfile::TempDir, Db, Warehouse) {
    let t = tempfile::tempdir().unwrap();
    let db = Db::open_with(t.path().join("m.redb"), topodb_json::default_spec()).unwrap();
    let wh = Warehouse::open(
        &t.path().join("w"),
        WarehouseConfig {
            spool_min_age_ms: 0,
            ..Default::default()
        },
    )
    .unwrap();
    (t, db, wh)
}

#[test]
fn derive_creates_artifacts_chunks_evidence_and_is_idempotent() {
    let (_t, db, mut wh) = setup();
    let m1 = memory(&db, "fact one");
    let long: String = (0..1500)
        .map(|i| format!("line {i} alpha beta gamma delta\n"))
        .collect();
    spool(
        &wh,
        "a.jsonl",
        &[
            marker("m0", 1, MarkerType::SessionStart, vec![]),
            art(
                "a1",
                2,
                "/src/lib.rs",
                &long,
                "Read",
                ArtifactType::FileRead,
            ),
            art(
                "a2",
                3,
                "cargo test",
                "ok 12 passed\n",
                "Bash",
                ArtifactType::Command,
            ),
            art(
                "a3",
                4,
                "/src/lib.rs",
                &long,
                "Read",
                ArtifactType::FileRead,
            ), // same content again
            marker("m1", 5, MarkerType::MemoryWrite, vec![m1.to_string()]),
        ],
    );
    wh.drain(10).unwrap();
    let embed = |t: &str| Some(("fake-embedder".to_string(), vec![t.len() as f32, 1.0, 2.0]));
    let rep = derive(&db, &wh, Some(&embed), 20, false).unwrap();
    assert_eq!(
        (
            rep.artifacts_created,
            rep.evidence_edges,
            rep.markers_skipped
        ),
        (2, 2, 0)
    );
    assert!(rep.chunks_created > 3);
    assert_eq!(rep.embed_model.as_deref(), Some("fake-embedder"));

    let scopes = topodb_json::scopes_to_scope_set(&[scope(), Scope::Shared]);
    let arts = db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL);
    assert_eq!(arts.len(), 2);
    let lib = arts
        .iter()
        .find(|a| matches!(a.props.get("locator"), Some(PropValue::Str(s)) if s == "/src/lib.rs"))
        .unwrap();
    assert_eq!(lib.props.get("seen_count"), Some(&PropValue::Int(2)));
    assert_eq!(
        lib.props.get("derived_by"),
        Some(&PropValue::Str(derived_by_stamp(Some("fake-embedder"))))
    );
    assert_eq!(lib.props.get("tier"), Some(&PropValue::Str("hot".into())));
    let chunks = db
        .edges_from(
            &scopes,
            lib.id,
            None,
            Some(HAS_CHUNK_EDGE),
            true,
            TimeAxis::Valid,
        )
        .unwrap();
    assert!(chunks.len() > 3);
    let c0 = db.node(&scopes, chunks[0].to).unwrap();
    assert_eq!(c0.label, CHUNK_LABEL);
    assert!(matches!(c0.props.get("text"), Some(PropValue::Str(s)) if s.starts_with("line ")));
    assert!(c0.embedding.is_some());
    // evidence: memory -> both artifacts, ranked
    let ev = db
        .edges_from(
            &scopes,
            m1,
            None,
            Some(EVIDENCE_EDGE),
            true,
            TimeAxis::Valid,
        )
        .unwrap();
    assert_eq!(ev.len(), 2);
    assert!(ev
        .iter()
        .all(|e| e.props.get("rule") == Some(&PropValue::Str("turn-window/1".into()))));

    // idempotent second run
    let rep2 = derive(&db, &wh, Some(&embed), 30, false).unwrap();
    assert_eq!(
        (
            rep2.artifacts_created,
            rep2.chunks_created,
            rep2.evidence_edges
        ),
        (0, 0, 0)
    );
    assert_eq!(db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL).len(), 2);
    assert_eq!(
        db.nodes_by_label_unbumped(&scopes, CHUNK_LABEL).len(),
        chunks.len() + 1
    ); // + the one command chunk
}

#[test]
fn rederive_replaces_derived_state_with_identical_ids_and_no_embedder_changes_stamp() {
    let (_t, db, mut wh) = setup();
    let m1 = memory(&db, "fact");
    spool(
        &wh,
        "a.jsonl",
        &[
            art(
                "a1",
                2,
                "x",
                "hello world\n",
                "Read",
                ArtifactType::FileRead,
            ),
            marker("m1", 3, MarkerType::MemoryWrite, vec![m1.to_string()]),
        ],
    );
    wh.drain(10).unwrap();
    let scopes = topodb_json::scopes_to_scope_set(&[scope(), Scope::Shared]);
    derive(&db, &wh, None, 20, false).unwrap();
    let a_before = db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL)[0].clone();
    assert_eq!(
        a_before.props.get("derived_by"),
        Some(&PropValue::Str("warehouse/1/none".into()))
    );
    let c_before = db.nodes_by_label_unbumped(&scopes, CHUNK_LABEL)[0].clone();
    assert!(c_before.embedding.is_none());
    let embed = |_t: &str| Some(("e2".to_string(), vec![0.5, 0.5]));
    let rep = derive(&db, &wh, Some(&embed), 40, true).unwrap();
    assert!(rep.removed >= 2);
    assert_eq!(rep.artifacts_created, 1);
    let a_after = db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL)[0].clone();
    assert_eq!(a_after.id, a_before.id);
    assert_eq!(
        a_after.props.get("derived_by"),
        Some(&PropValue::Str("warehouse/1/e2".into()))
    );
    let c_after = db.nodes_by_label_unbumped(&scopes, CHUNK_LABEL)[0].clone();
    assert_eq!(c_after.id, c_before.id);
    assert!(c_after.embedding.is_some());
    assert_eq!(
        db.edges_from(
            &scopes,
            m1,
            None,
            Some(EVIDENCE_EDGE),
            true,
            TimeAxis::Valid
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn derive_with_a_different_stamp_does_not_fail_or_recreate_without_rederive() {
    let (_t, db, mut wh) = setup();
    let m1 = memory(&db, "fact");
    spool(
        &wh,
        "a.jsonl",
        &[
            art(
                "a1",
                2,
                "x",
                "hello world\n",
                "Read",
                ArtifactType::FileRead,
            ),
            marker("m1", 3, MarkerType::MemoryWrite, vec![m1.to_string()]),
        ],
    );
    wh.drain(10).unwrap();
    let scopes = topodb_json::scopes_to_scope_set(&[scope(), Scope::Shared]);

    // First: plain CLI-style derive (no embedder) -> stamp warehouse/1/none.
    let rep1 = derive(&db, &wh, None, 20, false).unwrap();
    assert_eq!(rep1.artifacts_created, 1);
    let a_before = db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL)[0].clone();
    assert_eq!(
        a_before.props.get("derived_by"),
        Some(&PropValue::Str(derived_by_stamp(None)))
    );
    let chunks_before = db.nodes_by_label_unbumped(&scopes, CHUNK_LABEL).len();

    // Then: daemon-style derive with an embedder -> would-be stamp
    // warehouse/1/e2, but WITHOUT --rederive this must not fail or
    // remove/recreate the existing node.
    let embed = |_t: &str| Some(("e2".to_string(), vec![0.5, 0.5]));
    let rep2 = derive(&db, &wh, Some(&embed), 30, false).unwrap();
    assert_eq!(rep2.artifacts_created, 0);
    assert_eq!(rep2.removed, 0);
    let a_after = db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL)[0].clone();
    assert_eq!(a_after.id, a_before.id);
    assert_eq!(
        a_after.props.get("derived_by"),
        Some(&PropValue::Str(derived_by_stamp(None))),
        "stamp must NOT migrate without --rederive"
    );
    assert_eq!(
        db.nodes_by_label_unbumped(&scopes, CHUNK_LABEL).len(),
        chunks_before,
        "chunk count unchanged"
    );

    // Finally: rederive=true migrates the stamp (existing rederive semantics).
    let rep3 = derive(&db, &wh, Some(&embed), 40, true).unwrap();
    assert!(rep3.removed >= 1);
    let a_final = db.nodes_by_label_unbumped(&scopes, ARTIFACT_LABEL)[0].clone();
    assert_eq!(
        a_final.props.get("derived_by"),
        Some(&PropValue::Str(derived_by_stamp(Some("e2"))))
    );
}

#[test]
fn pointer_only_artifacts_get_a_node_but_no_chunks_and_unknown_memory_ids_are_skipped() {
    let (_t, db, mut wh) = setup();
    wh.config.max_artifact_kb = 0; // everything pointer-only
    spool(
        &wh,
        "a.jsonl",
        &[
            art("a1", 2, "big", "content\n", "Read", ArtifactType::FileRead),
            marker(
                "m1",
                3,
                MarkerType::MemoryWrite,
                vec![NodeId::new().to_string()],
            ),
        ],
    );
    wh.drain(10).unwrap();
    let rep = derive(&db, &wh, None, 20, false).unwrap();
    assert_eq!(
        (
            rep.artifacts_created,
            rep.pointer_only,
            rep.chunks_created,
            rep.evidence_edges,
            rep.markers_skipped
        ),
        (1, 1, 0, 0, 1)
    );
}
