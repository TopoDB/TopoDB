//! plan_remember against a real (temp) engine Db. Every plan's `ops` are
//! submitted through `db.submit` exactly as a front end would.

use topodb::{Db, PropValue, Scope};
use topodb_json::{
    content_hash, default_spec, plan_remember, scopes_to_scope_set, ComposeError, RememberRequest,
    MEMORY_SUPERSEDED_AT_PROP,
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
    };
    let err = r.validate().unwrap_err();
    assert!(err.contains("entities must contain"), "{err}");
}

#[test]
fn validate_rejects_blank_entity_names() {
    let r = RememberRequest {
        content: "x".into(),
        entities: vec!["  ".into()],
        edge_type: None,
        supersedes: vec![],
        props: None,
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
    for key in ["content_hash", "superseded_at"] {
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
