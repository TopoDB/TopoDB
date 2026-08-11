//! plan_remember against a real (temp) engine Db. Every plan's `ops` are
//! submitted through `db.submit` exactly as a front end would.

use topodb::{Db, Op, PropValue, Scope, TimeAxis};
use topodb_json::{
    content_hash, default_spec, memory_props, plan_remember, scopes_to_scope_set,
    validate_memory_kind, ComposeError, RememberRequest, MEMORY_SUPERSEDED_AT_PROP,
};

fn fresh_db(dir: &tempfile::TempDir) -> Db {
    Db::open_with(dir.path().join("t.redb"), default_spec()).unwrap()
}

fn req(content: &str, entities: &[&str]) -> RememberRequest {
    RememberRequest {
        content: content.into(),
        entities: entities.iter().map(|s| s.to_string()).collect(),
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: None,
    }
}

fn lookup() -> topodb::ScopeSet {
    scopes_to_scope_set(&[Scope::Shared])
}

#[test]
fn fresh_remember_plans_memory_entities_and_links() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let plan = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("ada wrote it", &["ada"]),
    )
    .unwrap();
    assert!(!plan.deduplicated);
    assert_eq!(plan.entities.len(), 1);
    assert!(plan.entities[0].created);
    assert_eq!(plan.edge_ids.len(), 1);
    assert_eq!(plan.new_entities.len(), 1);
    assert_eq!(plan.new_memory.as_deref(), Some("ada wrote it"));
    db.submit(plan.ops).unwrap();
    // The memory node exists with content + content_hash.
    let node = db.node(&lookup(), plan.memory_id).expect("memory node");
    assert_eq!(node.props["content"], PropValue::Str("ada wrote it".into()));
    assert!(node.props.contains_key("content_hash"));
}

#[test]
fn identical_remember_dedups_to_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let first = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("ada wrote it", &["ada"]),
    )
    .unwrap();
    db.submit(first.ops).unwrap();
    let second = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        2_000,
        &req("ada  wrote it", &["ada"]),
    )
    .unwrap();
    assert!(
        second.deduplicated,
        "whitespace-normalized content must dedup"
    );
    assert_eq!(second.memory_id, first.memory_id);
    assert!(
        second.ops.is_empty(),
        "dedup hit with same entity must plan no writes"
    );
    assert_eq!(
        second.edge_ids, first.edge_ids,
        "existing edge id is echoed"
    );
    assert!(second.new_memory.is_none());
}

#[test]
fn entity_is_reused_across_composes() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let first = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("fact one", &["vega"]),
    )
    .unwrap();
    db.submit(first.ops).unwrap();
    let second = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        2_000,
        &req("fact two", &["vega"]),
    )
    .unwrap();
    assert!(
        !second.entities[0].created,
        "same-name entity must be found, not recreated"
    );
    assert_eq!(second.entities[0].id, first.entities[0].id);
    db.submit(second.ops).unwrap();
}

#[test]
fn in_call_name_variants_collapse_to_one_entity() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let plan = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("a fact", &["Ada Lovelace", " ada   lovelace "]),
    )
    .unwrap();
    assert_eq!(plan.entities.len(), 1);
    assert_eq!(plan.entities[0].name, "Ada Lovelace", "first spelling wins");
    assert_eq!(plan.edge_ids.len(), 1);
}

#[test]
fn supersedes_stamps_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let old = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("db is postgres", &["vega"]),
    )
    .unwrap();
    db.submit(old.ops).unwrap();
    let mut new_req = req("db is sqlite", &["vega"]);
    new_req.supersedes = vec![old.memory_id.to_string()];
    let new = plan_remember(&db, Scope::Shared, &lookup(), 5_000, &new_req).unwrap();
    assert_eq!(new.superseded, vec![old.memory_id.to_string()]);
    db.submit(new.ops).unwrap();
    let node = db.node(&lookup(), old.memory_id).unwrap();
    assert_eq!(node.props[MEMORY_SUPERSEDED_AT_PROP], PropValue::Int(5_000));
    // Re-superseding the same id is a no-op, not a re-stamp.
    let mut again = req("db is sqlite v2", &["vega"]);
    again.supersedes = vec![old.memory_id.to_string()];
    let plan = plan_remember(&db, Scope::Shared, &lookup(), 9_000, &again).unwrap();
    assert!(plan.superseded.is_empty());
    db.submit(plan.ops).unwrap();
    let node = db.node(&lookup(), old.memory_id).unwrap();
    assert_eq!(node.props[MEMORY_SUPERSEDED_AT_PROP], PropValue::Int(5_000));
}

#[test]
fn foreign_or_non_memory_supersedes_id_is_invalid_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("a fact", &["vega"]),
    )
    .unwrap();
    let entity_id = seeded.entities[0].id;
    db.submit(seeded.ops).unwrap();
    let seq_before = db.current_seq().unwrap();
    // Unknown id.
    let mut r = req("newer fact", &["vega"]);
    r.supersedes = vec![topodb::NodeId::new().to_string()];
    assert!(matches!(
        plan_remember(&db, Scope::Shared, &lookup(), 2_000, &r),
        Err(ComposeError::Invalid(_))
    ));
    // An Entity, not a Memory.
    let mut r = req("newer fact", &["vega"]);
    r.supersedes = vec![entity_id.to_string()];
    match plan_remember(&db, Scope::Shared, &lookup(), 2_000, &r) {
        Err(ComposeError::Invalid(msg)) => assert!(msg.contains("not a Memory"), "{msg}"),
        other => panic!("expected Invalid, got {:?}", other.map(|p| p.memory_id)),
    }
    assert_eq!(
        db.current_seq().unwrap(),
        seq_before,
        "a rejected plan must write nothing"
    );
}

#[test]
fn empty_entities_and_blank_names_are_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    assert!(matches!(
        plan_remember(&db, Scope::Shared, &lookup(), 1_000, &req("x", &[])),
        Err(ComposeError::Invalid(_))
    ));
    assert!(matches!(
        plan_remember(&db, Scope::Shared, &lookup(), 1_000, &req("x", &["  "])),
        Err(ComposeError::Invalid(_))
    ));
}

#[test]
fn edge_type_is_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let mut r = req("a fact", &["vega"]);
    r.edge_type = Some("Works At".into());
    let plan = plan_remember(&db, Scope::Shared, &lookup(), 1_000, &r).unwrap();
    let has_normalized = plan.ops.iter().any(|op| {
        matches!(
            op, topodb::Op::CreateEdge { ty, .. } if *ty == "works_at"
        )
    });
    assert!(has_normalized, "edge type must normalize to works_at");
}

#[test]
fn content_hash_is_whitespace_stable_and_case_sensitive() {
    assert_eq!(content_hash("a  b"), content_hash(" a b "));
    assert_ne!(content_hash("a b"), content_hash("A b"));
}

#[test]
fn validate_rejects_empty_entities() {
    let r = RememberRequest {
        content: "x".into(),
        entities: vec![],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: None,
    };
    let err = r.validate().unwrap_err();
    assert!(err.contains("entities must contain"), "{err}");
}

#[test]
fn self_supersede_mints_fresh_not_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    // First: remember X with entity "vega"
    let first = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("the fact", &["vega"]),
    )
    .unwrap();
    assert!(!first.deduplicated);
    let old_id = first.memory_id;
    db.submit(first.ops).unwrap();

    // Second: remember the SAME content with supersedes=[old id] and entity "mira"
    let mut second_req = req("the fact", &["mira"]);
    second_req.supersedes = vec![old_id.to_string()];
    let second = plan_remember(&db, Scope::Shared, &lookup(), 2_000, &second_req).unwrap();

    // Must NOT deduplicate (fresh node, not reuse old)
    assert!(
        !second.deduplicated,
        "remembering X with supersedes=[id of live X] must mint fresh, not dedup"
    );
    // Fresh memory_id must differ from the old one
    assert_ne!(
        second.memory_id, old_id,
        "self-supersede must create a new memory node"
    );
    // Must mark the old one as superseded
    assert_eq!(
        second.superseded,
        vec![old_id.to_string()],
        "old id must be in superseded list"
    );

    // Submit and verify: old node has superseded_at, fresh node exists
    db.submit(second.ops).unwrap();
    let old_node = db.node(&lookup(), old_id).unwrap();
    assert_eq!(
        old_node.props[MEMORY_SUPERSEDED_AT_PROP],
        PropValue::Int(2_000),
        "old node must have superseded_at timestamp"
    );
    // Check old node has no open out-edges (all should be closed)
    let old_edges = db
        .edges_from(&lookup(), old_id, None, None, true, TimeAxis::Valid)
        .unwrap();
    assert!(
        old_edges.is_empty(),
        "old node should have no open out-edges after supersede"
    );

    let fresh_node = db.node(&lookup(), second.memory_id).unwrap();
    assert_eq!(
        fresh_node.props["content"],
        PropValue::Str("the fact".into()),
        "fresh node must have the content"
    );
    // Verify the fresh node has an edge to mira
    let fresh_edges = db
        .edges_from(
            &lookup(),
            second.memory_id,
            None,
            Some("about"),
            true,
            TimeAxis::Valid,
        )
        .unwrap();
    assert_eq!(fresh_edges.len(), 1, "fresh node must have edge to mira");
}

#[test]
fn validate_rejects_blank_entity_names() {
    let r = RememberRequest {
        content: "x".into(),
        entities: vec!["  ".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: None,
    };
    let err = r.validate().unwrap_err();
    assert!(err.contains("entity names must be non-empty"), "{err}");
}

#[test]
fn validate_normalizes_default_edge_type() {
    let r = RememberRequest {
        content: "x".into(),
        entities: vec!["one".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: None,
    };
    let ty = r.validate().unwrap();
    assert_eq!(ty, "about");
}

#[test]
fn validate_succeeds_with_valid_entity() {
    let r = RememberRequest {
        content: "x".into(),
        entities: vec!["one".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: None,
    };
    assert_eq!(r.validate().unwrap(), "about");
}

#[test]
fn superseded_content_does_not_dedup_and_mints_a_fresh_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let old = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("db is postgres", &["vega"]),
    )
    .unwrap();
    db.submit(old.ops).unwrap();
    let mut sup = req("db is sqlite", &["vega"]);
    sup.supersedes = vec![old.memory_id.to_string()];
    db.submit(
        plan_remember(&db, Scope::Shared, &lookup(), 2_000, &sup)
            .unwrap()
            .ops,
    )
    .unwrap();
    // Re-remember the retired content: must NOT dedup to the tombstone.
    let again = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        3_000,
        &req("db is postgres", &["vega"]),
    )
    .unwrap();
    assert!(!again.deduplicated, "superseded content must not dedup");
    assert_ne!(
        again.memory_id, old.memory_id,
        "fresh live memory, not the tombstone"
    );
    db.submit(again.ops).unwrap();
    // Tombstone untouched; new node has no stamp.
    let tomb = db.node(&lookup(), old.memory_id).unwrap();
    assert_eq!(tomb.props[MEMORY_SUPERSEDED_AT_PROP], PropValue::Int(2_000));
    let fresh = db.node(&lookup(), again.memory_id).unwrap();
    assert!(!fresh.props.contains_key(MEMORY_SUPERSEDED_AT_PROP));
}

#[test]
fn alias_name_resolves_to_canonical_entity() {
    use topodb_json::{ALIAS_EDGE_TYPE, ALIAS_LABEL, ALIAS_NAME_PROP};
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("vega exists", &["vega"]),
    )
    .unwrap();
    let canonical = seeded.entities[0].id;
    db.submit(seeded.ops).unwrap();
    // Seed an Alias node + alias_of edge via raw ops.
    let alias_id = topodb::NodeId::new();
    let mut props = topodb::Props::new();
    props.insert(
        ALIAS_NAME_PROP.into(),
        PropValue::Str("the vega project".into()),
    );
    db.submit(vec![
        topodb::Op::CreateNode {
            id: alias_id,
            scope: Scope::Shared,
            label: ALIAS_LABEL.into(),
            props,
        },
        topodb::Op::CreateEdge {
            id: topodb::EdgeId::new(),
            scope: Scope::Shared,
            ty: ALIAS_EDGE_TYPE.into(),
            from: alias_id,
            to: canonical,
            props: topodb::Props::new(),
            valid_from: None,
            recorded_at: None,
        },
    ])
    .unwrap();
    // Remember via the ALIAS name: must resolve to the canonical entity.
    let plan = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        2_000,
        &req("a fact", &["the vega project"]),
    )
    .unwrap();
    assert!(
        !plan.entities[0].created,
        "alias must resolve, not mint a duplicate"
    );
    assert_eq!(plan.entities[0].id, canonical);
    assert_eq!(plan.edge_ids.len(), 1);
}

#[test]
fn memory_props_rejects_reserved_keys_and_stamps_hash() {
    use topodb_json::memory_props;
    for key in ["content_hash", "superseded_at", "forgotten_at"] {
        let extra = serde_json::json!({ key: "boom" });
        let err = memory_props("a fact", Some(&extra)).unwrap_err();
        assert!(err.contains(key), "error must name the reserved key: {err}");
        assert!(err.contains("maintained by the engine write path"), "{err}");
    }
    // `content` collision still rejected via merge_required_prop.
    assert!(memory_props("a fact", Some(&serde_json::json!({"content": "x"}))).is_err());
    // Happy path: content + stamped hash + extra key.
    let props = memory_props("a fact", Some(&serde_json::json!({"source": "chat"}))).unwrap();
    assert_eq!(props["content"], PropValue::Str("a fact".into()));
    assert_eq!(
        props["content_hash"],
        PropValue::Str(content_hash("a fact"))
    );
    assert_eq!(props["source"], PropValue::Str("chat".into()));
}

#[test]
fn plan_forget_stamps_and_closes_edges_for_a_live_memory() {
    use topodb_json::plan_forget;
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("a memory", &["entity"]),
    )
    .unwrap();
    let m = seeded.memory_id;
    db.submit(seeded.ops).unwrap();
    let scope = Scope::Shared;
    let (ops, forgotten) = plan_forget(&db, scope, &[m.to_string()], 5_000).unwrap();
    assert_eq!(forgotten, vec![m.to_string()]);
    assert!(matches!(
        &ops[0],
        topodb::Op::SetNodeProps { id, props }
            if id == &m && props.get("forgotten_at") == Some(&Some(PropValue::Int(5_000)))
    ));
    assert!(
        ops.iter()
            .skip(1)
            .all(|op| matches!(op, topodb::Op::CloseEdge { .. })),
        "everything after the stamp closes an open edge"
    );
    assert!(ops.len() >= 2, "the memory->entity edge must be closed");
}

#[test]
fn plan_forget_rejects_every_invalid_target_before_building_ops() {
    use topodb_json::plan_forget;
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("live memory", &["entity"]),
    )
    .unwrap();
    let m = seeded.memory_id;
    let e = seeded.entities[0].id;
    db.submit(seeded.ops).unwrap();
    // Create a forgotten memory for testing
    let forg_seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        2_000,
        &req("forgotten memory", &["entity"]),
    )
    .unwrap();
    let forg = forg_seeded.memory_id;
    db.submit(forg_seeded.ops).unwrap();
    let (forg_ops, _) = plan_forget(&db, Scope::Shared, &[forg.to_string()], 3_000).unwrap();
    db.submit(forg_ops).unwrap();
    // Create a superseded memory for testing
    let sup_seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        4_000,
        &req("superseded memory", &["entity"]),
    )
    .unwrap();
    let sup = sup_seeded.memory_id;
    db.submit(sup_seeded.ops).unwrap();
    let mut sup_req = req("newer version", &["entity"]);
    sup_req.supersedes = vec![sup.to_string()];
    let sup_plan = plan_remember(&db, Scope::Shared, &lookup(), 5_000, &sup_req).unwrap();
    db.submit(sup_plan.ops).unwrap();
    let scope = Scope::Shared;
    for (ids, needle) in [
        (vec!["not-a-ulid".to_string()], "invalid node id"),
        (
            vec![topodb::NodeId::new().to_string()],
            "not a node in the write scope",
        ),
        (vec![e.to_string()], "not a Memory"),
        (vec![forg.to_string()], "already forgotten"),
        (vec![sup.to_string()], "already superseded"),
        // one bad id poisons the whole call — atomicity of judgment
        (vec![m.to_string(), e.to_string()], "not a Memory"),
    ] {
        match plan_forget(&db, scope, &ids, 5_000) {
            Err(ComposeError::Invalid(msg)) => {
                assert!(msg.contains(needle), "{needle:?} not in {msg:?}")
            }
            other => panic!("expected Invalid({needle}), got {other:?}"),
        }
    }
}

#[test]
fn plan_forget_rejects_empty_and_dedups_repeats() {
    use topodb_json::plan_forget;
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let seeded = plan_remember(
        &db,
        Scope::Shared,
        &lookup(),
        1_000,
        &req("a memory", &["entity"]),
    )
    .unwrap();
    let m = seeded.memory_id;
    db.submit(seeded.ops).unwrap();
    let scope = Scope::Shared;
    assert!(matches!(
        plan_forget(&db, scope, &[], 5_000),
        Err(ComposeError::Invalid(msg)) if msg.contains("at least one")
    ));
    let (_, forgotten) = plan_forget(&db, scope, &[m.to_string(), m.to_string()], 5_000).unwrap();
    assert_eq!(
        forgotten,
        vec![m.to_string()],
        "repeat of the same id is one forget"
    );
}

#[test]
fn validate_memory_kind_accepts_the_enum_and_rejects_everything_else() {
    for ok in ["episodic", "semantic", "procedural", "decision"] {
        assert!(validate_memory_kind(ok).is_ok(), "{ok} must validate");
    }
    for bad in ["", "Episodic", "SEMANTIC", "Decision", "factual", "kind"] {
        let err = validate_memory_kind(bad).unwrap_err();
        assert!(
            err.contains("episodic") && err.contains(&format!("{bad:?}")),
            "message must name the vocabulary and the bad value: {err}"
        );
    }
}

/// The vocabulary is closed at FOUR kinds and the rejection message names
/// every one of them — a fifth kind added to the array without updating the
/// message (or vice versa) fails here.
#[test]
fn decision_kind_is_in_the_vocabulary_and_the_error_lists_all_four() {
    assert!(topodb_json::MEMORY_KINDS.contains(&"decision"));
    assert_eq!(
        topodb_json::MEMORY_KIND_DEFAULT,
        "semantic",
        "adding decision must not move the absent-kind default"
    );
    let err = validate_memory_kind("resolution").unwrap_err();
    for kind in topodb_json::MEMORY_KINDS {
        assert!(err.contains(kind), "message must list {kind:?}, got: {err}");
    }
}

#[test]
fn plan_remember_stamps_decision_kind_on_new_memories() {
    // fixture: empty db + write scope, as the suite's other plan_remember
    // tests build them.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let lookup = lookup();

    let mut request = req("ship semantica as one lean PR", &["semantica"]);
    request.kind = Some("decision".into());
    let plan = plan_remember(&db, Scope::Shared, &lookup, 5_000, &request).unwrap();
    let create = plan
        .ops
        .iter()
        .find_map(|op| match op {
            Op::CreateNode { id, props, .. } if *id == plan.memory_id => Some(props),
            _ => None,
        })
        .expect("a new memory CreateNode");
    assert_eq!(
        create.get("kind"),
        Some(&PropValue::Str("decision".into())),
        "decision must stamp like every other kind"
    );
}

#[test]
fn memory_props_rejects_kind_as_reserved() {
    let err = memory_props(
        "some fact",
        Some(&serde_json::json!({ "kind": "episodic" })),
    )
    .unwrap_err();
    assert!(
        err.contains("\"kind\"") && err.contains("kind parameter"),
        "the rejection must point at the kind parameter, got: {err}"
    );
}

#[test]
fn plan_remember_stamps_kind_on_new_memories_and_validates_it() {
    // fixture: empty db + write scope, as the suite's other plan_remember
    // tests build them.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let scope = Scope::Shared;
    let lookup = lookup();

    let req = RememberRequest {
        content: "vega uses grpc".into(),
        entities: vec!["vega".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: Some("episodic".into()),
    };
    let plan = plan_remember(&db, scope, &lookup, 5_000, &req).unwrap();
    let create = plan
        .ops
        .iter()
        .find_map(|op| match op {
            Op::CreateNode { id, props, .. } if *id == plan.memory_id => Some(props),
            _ => None,
        })
        .expect("a new memory CreateNode");
    assert_eq!(
        create.get("kind"),
        Some(&PropValue::Str("episodic".into())),
        "kind must be stamped on the new memory's props"
    );

    // An invalid kind rejects before any planning.
    let bad = RememberRequest {
        content: "vega uses grpc".into(),
        entities: vec!["vega".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: Some("factual".into()),
    };
    match plan_remember(&db, scope, &lookup, 5_000, &bad) {
        Err(ComposeError::Invalid(m)) => assert!(m.contains("episodic"), "{m}"),
        Err(ComposeError::Engine(_)) => panic!("expected Invalid error, got Engine error"),
        Ok(_) => panic!("expected Invalid error, got Ok"),
    }
}

/// Dedup ignores kind: same content with a DIFFERENT declared kind still
/// dedups to the existing memory, and the stored kind wins (no SetNodeProps
/// touches the existing node's kind).
#[test]
fn plan_remember_dedup_ignores_kind_and_stored_kind_wins() {
    // fixture: empty db + write scope + lookup, as above.
    let dir = tempfile::tempdir().unwrap();
    let db = fresh_db(&dir);
    let scope = Scope::Shared;
    let lookup = lookup();

    let first = RememberRequest {
        content: "lyra uses mqtt".into(),
        entities: vec!["lyra".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: Some("procedural".into()),
    };
    let plan1 = plan_remember(&db, scope, &lookup, 5_000, &first).unwrap();
    db.submit(plan1.ops).unwrap();

    let second = RememberRequest {
        content: "lyra uses mqtt".into(),
        entities: vec!["lyra".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
        kind: Some("episodic".into()),
    };
    let plan2 = plan_remember(&db, scope, &lookup, 6_000, &second).unwrap();
    assert!(
        plan2.deduplicated,
        "identical content must dedup regardless of kind"
    );
    assert_eq!(plan2.memory_id, plan1.memory_id);
    assert!(
        plan2.ops.iter().all(|op| !matches!(
            op,
            Op::SetNodeProps { id, .. } if *id == plan1.memory_id
        )),
        "the dedup hit's stored kind must win — no prop rewrite"
    );
}
