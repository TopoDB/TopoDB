//! Per-note ingest planner.

use crate::NoteInput;
use topodb::{Db, NodeId, Op, PropValue, Scope, ScopeSet};
use topodb_json::{
    entity_dedup_key, existing_memory, json_to_props, memory_props, normalize_content,
    plan_remember, plan_supersede, ComposeError, RememberRequest, ENTITY_LABEL,
    MEMORY_CONTENT_HASH_PROP, MEMORY_CONTENT_PROP, MEMORY_FORGOTTEN_AT_PROP, MEMORY_KINDS,
    MEMORY_KIND_PROP, MEMORY_LABEL, MEMORY_SUPERSEDED_AT_PROP,
};

pub enum NoteAction {
    Created { memory_id: NodeId },
    Superseded { memory_id: NodeId, old: String },
    Deduplicated { memory_id: NodeId },
    SkippedUnchanged,
    SkippedEntityStub,
}

pub struct IngestOutcome {
    pub action: NoteAction,
    pub ops: Vec<topodb::Op>,
    pub new_memory: Option<String>, // content, for the embed hook
    pub new_entities: Vec<(NodeId, String)>, // for the embed hook
}

pub fn plan_note(
    db: &Db,
    write_scope: Scope,
    lookup: &ScopeSet,
    now_ms: i64,
    input: &NoteInput,
) -> Result<IngestOutcome, ComposeError> {
    if input.is_entity_stub {
        return Ok(skip(NoteAction::SkippedEntityStub));
    }
    if let Some(kind) = &input.kind {
        if !MEMORY_KINDS.contains(&kind.as_str()) {
            return Err(ComposeError::Invalid(format!(
                "invalid kind {kind:?} (episodic | semantic | procedural)"
            )));
        }
    }
    if input.content.is_empty() {
        return Err(ComposeError::Invalid("note body is empty".into()));
    }
    let supersedes: Vec<String> = match &input.id {
        None => Vec::new(),
        Some(raw) => {
            let id: NodeId = raw
                .parse()
                .map_err(|e| ComposeError::Invalid(format!("invalid topodb-id {raw:?}: {e}")))?;
            let Some(node) = db.node(lookup, id) else {
                return Err(ComposeError::Invalid(format!(
                    "topodb-id {raw} not found in this database"
                )));
            };
            if node.label.as_str() == ENTITY_LABEL {
                return Ok(skip(NoteAction::SkippedEntityStub));
            }
            if node.label.as_str() != MEMORY_LABEL {
                return Err(ComposeError::Invalid(format!(
                    "topodb-id {raw} is a {} node, not a Memory",
                    node.label
                )));
            }
            if unchanged(db, lookup, &node, input)? {
                return Ok(skip(NoteAction::SkippedUnchanged));
            }
            vec![raw.clone()]
        }
    };

    if input.entities.is_empty() {
        return plan_without_entities(db, write_scope, now_ms, input, supersedes);
    }
    let req = RememberRequest {
        content: input.content.clone(),
        entities: input.entities.clone(),
        edge_type: None,
        supersedes: supersedes.clone(),
        props: input.props.clone(),
        kind: input.kind.clone(),
    };
    let plan = plan_remember(db, write_scope, lookup, now_ms, &req)?;
    let action = if let Some(old) = supersedes.into_iter().next() {
        NoteAction::Superseded {
            memory_id: plan.memory_id,
            old,
        }
    } else if plan.deduplicated {
        NoteAction::Deduplicated {
            memory_id: plan.memory_id,
        }
    } else {
        NoteAction::Created {
            memory_id: plan.memory_id,
        }
    };
    Ok(IngestOutcome {
        action,
        ops: plan.ops,
        new_memory: plan.new_memory,
        new_entities: plan.new_entities,
    })
}

fn plan_without_entities(
    db: &Db,
    write_scope: Scope,
    now_ms: i64,
    input: &NoteInput,
    supersedes: Vec<String>,
) -> Result<IngestOutcome, ComposeError> {
    if supersedes.is_empty() {
        if let Some(existing) = existing_memory(db, write_scope, &input.content)? {
            return Ok(IngestOutcome {
                action: NoteAction::Deduplicated {
                    memory_id: existing,
                },
                ops: Vec::new(),
                new_memory: None,
                new_entities: Vec::new(),
            });
        }
    }
    let mut props =
        memory_props(&input.content, input.props.as_ref()).map_err(ComposeError::Invalid)?;
    if let Some(kind) = &input.kind {
        props.insert(MEMORY_KIND_PROP.into(), PropValue::Str(kind.clone()));
    }
    let memory_id = NodeId::new();
    let mut ops = vec![Op::CreateNode {
        id: memory_id,
        scope: write_scope,
        label: MEMORY_LABEL.into(),
        props,
    }];
    let action = if let Some(old) = supersedes.first().cloned() {
        let (sup_ops, _) = plan_supersede(db, write_scope, &supersedes, now_ms)?;
        ops.extend(sup_ops);
        NoteAction::Superseded { memory_id, old }
    } else {
        NoteAction::Created { memory_id }
    };
    Ok(IngestOutcome {
        action,
        ops,
        new_memory: Some(input.content.clone()),
        new_entities: Vec::new(),
    })
}

/// Content, kind, props, and linked-entity set all equal → no-op.
fn unchanged(
    db: &Db,
    lookup: &ScopeSet,
    node: &topodb::NodeRecord,
    input: &NoteInput,
) -> Result<bool, ComposeError> {
    let stored = node
        .props
        .get(MEMORY_CONTENT_PROP)
        .and_then(|v| match v {
            PropValue::Str(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");
    if normalize_content(stored) != normalize_content(&input.content) {
        return Ok(false);
    }
    let stored_kind = node.props.get(MEMORY_KIND_PROP).and_then(|v| match v {
        PropValue::Str(s) => Some(s.clone()),
        _ => None,
    });
    if stored_kind != input.kind {
        return Ok(false);
    }
    let expected = match &input.props {
        None => topodb::Props::new(),
        Some(v) => json_to_props(v).map_err(ComposeError::Invalid)?,
    };
    let mut stored_user: topodb::Props = node.props.clone();
    for k in [
        MEMORY_CONTENT_PROP,
        MEMORY_CONTENT_HASH_PROP,
        MEMORY_KIND_PROP,
        MEMORY_SUPERSEDED_AT_PROP,
        MEMORY_FORGOTTEN_AT_PROP,
    ] {
        stored_user.remove(k);
    }
    if stored_user != expected {
        return Ok(false);
    }
    // Entity links: open out-edges → Entity names, compared as dedup keys.
    let edges = db.edges_from(lookup, node.id, None, None, true)?;
    let mut stored_ents = std::collections::BTreeSet::new();
    for e in edges {
        if let Some(n) = db.node(lookup, e.to) {
            if n.label.as_str() == ENTITY_LABEL {
                if let Some(PropValue::Str(name)) = n.props.get(topodb_json::ENTITY_NAME_PROP) {
                    stored_ents.insert(entity_dedup_key(name));
                }
            }
        }
    }
    let note_ents: std::collections::BTreeSet<String> =
        input.entities.iter().map(|e| entity_dedup_key(e)).collect();
    Ok(stored_ents == note_ents)
}

fn skip(action: NoteAction) -> IngestOutcome {
    IngestOutcome {
        action,
        ops: Vec::new(),
        new_memory: None,
        new_entities: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{note_to_input, Note};
    use topodb::{Db, Scope};
    use topodb_json::scopes_to_scope_set;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("t.redb"), topodb_json::default_spec()).unwrap();
        (dir, db)
    }
    fn input(src: &str) -> crate::NoteInput {
        note_to_input(&Note::parse(src).unwrap()).unwrap()
    }
    fn plan(db: &Db, src: &str) -> IngestOutcome {
        let lookup = scopes_to_scope_set(&[Scope::Shared]);
        plan_note(db, Scope::Shared, &lookup, 1_000, &input(src)).unwrap()
    }

    #[test]
    fn new_note_with_links_creates_via_plan_remember() {
        let (_d, db) = db();
        let out = plan(&db, "---\nkind: episodic\n---\nUse [[redb]].\n");
        // NOTE: kind values are strictly episodic|semantic|procedural
        // (MEMORY_KINDS); anything else is ComposeError::Invalid.
        assert!(matches!(out.action, NoteAction::Created { .. }));
        assert_eq!(out.new_entities.len(), 1);
        assert_eq!(out.new_memory.as_deref(), Some("Use [[redb]]."));
        db.submit(out.ops).unwrap();
    }

    #[test]
    fn new_note_without_links_creates_manually() {
        let (_d, db) = db();
        let out = plan(&db, "no links here\n");
        assert!(matches!(out.action, NoteAction::Created { .. }));
        assert!(out.new_entities.is_empty());
        db.submit(out.ops).unwrap();
        // Same content again → dedup, no ops.
        let again = plan(&db, "no links here\n");
        assert!(matches!(again.action, NoteAction::Deduplicated { .. }));
        assert!(again.ops.is_empty());
    }

    #[test]
    fn unchanged_note_with_id_is_noop_and_changed_supersedes() {
        let (_d, db) = db();
        let first = plan(&db, "---\nstatus: open\n---\nFact one about [[redb]].\n");
        let NoteAction::Created { memory_id } = first.action else {
            panic!()
        };
        db.submit(first.ops).unwrap();

        let unchanged = plan(
            &db,
            &format!("---\ntopodb-id: {memory_id}\nstatus: open\n---\nFact one about [[redb]].\n"),
        );
        assert!(matches!(unchanged.action, NoteAction::SkippedUnchanged));
        assert!(unchanged.ops.is_empty());

        let changed = plan(
            &db,
            &format!(
                "---\ntopodb-id: {memory_id}\nstatus: closed\n---\nFact one about [[redb]].\n"
            ),
        );
        let NoteAction::Superseded {
            memory_id: new_id,
            old,
        } = changed.action
        else {
            panic!()
        };
        assert_eq!(old, memory_id.to_string());
        assert_ne!(new_id, memory_id);
        db.submit(changed.ops).unwrap();
        let scopes = scopes_to_scope_set(&[Scope::Shared]);
        let old_node = db.node(&scopes, memory_id).unwrap();
        assert!(old_node
            .props
            .contains_key(topodb_json::MEMORY_SUPERSEDED_AT_PROP));
    }

    #[test]
    fn entity_stub_and_bad_ids_are_handled() {
        let (_d, db) = db();
        // Stub flag short-circuits.
        let stub = plan(
            &db,
            "---\ntopodb-id: 01J9Z6S3V0AAAAAAAAAAAAAAAA\nentity: true\n---\n",
        );
        assert!(matches!(stub.action, NoteAction::SkippedEntityStub));
        // Unknown id → Invalid.
        let lookup = scopes_to_scope_set(&[Scope::Shared]);
        let missing = plan_note(
            &db,
            Scope::Shared,
            &lookup,
            0,
            &input("---\ntopodb-id: 01J9Z6S3V0AAAAAAAAAAAAAAAA\n---\nx\n"),
        );
        assert!(matches!(
            missing,
            Err(topodb_json::ComposeError::Invalid(_))
        ));
        // Id resolving to an Entity node → SkippedEntityStub.
        let seeded = plan(&db, "About [[Widget]].\n");
        db.submit(seeded.ops).unwrap();
        let widget = topodb_json::find_existing_entity(&db, &lookup, "Widget")
            .unwrap()
            .unwrap();
        let ent = plan(
            &db,
            &format!("---\ntopodb-id: {}\n---\nedited\n", widget.id),
        );
        assert!(matches!(ent.action, NoteAction::SkippedEntityStub));
    }
}
