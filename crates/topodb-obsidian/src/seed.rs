//! Seed rendering: memory notes + entity stubs, non-clobber writes.
//! Selection (Task 9) filters `memories` before this runs.

use crate::{Note, RELATED_KEY, TITLE_PROP, TOPODB_ID_KEY};
use serde_yaml::{Mapping, Value as Yaml};
use std::collections::BTreeMap;
use topodb::{Db, Direction, NodeId, NodeRecord, PropValue, RecallQuery, ScopeSet, TraversalQuery};
use topodb_json::{
    find_existing_entity, ComposeError, ALIAS_EDGE_TYPE, ALIAS_NAME_PROP, ENTITY_LABEL,
    ENTITY_NAME_PROP, MEMORY_CONTENT_HASH_PROP, MEMORY_CONTENT_PROP, MEMORY_FORGOTTEN_AT_PROP,
    MEMORY_LABEL, MEMORY_SUPERSEDED_AT_PROP, MEMORY_TOMBSTONE_PROPS,
};

/// Select live memories by BM25/vector recall (tombstoned memories excluded).
pub fn select_by_query(
    db: &Db,
    scopes: &ScopeSet,
    query: &str,
    k: usize,
    vector: Option<(String, Vec<f32>)>,
) -> Result<Vec<NodeRecord>, topodb::TopoError> {
    let q = RecallQuery {
        vector,
        labels: Some(vec![MEMORY_LABEL.to_string()]),
        tombstone_props: MEMORY_TOMBSTONE_PROPS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ..RecallQuery::new(scopes.clone(), query, k)
    };
    Ok(db.recall(&q)?.into_iter().map(|(n, _)| n).collect())
}

/// Select live memories within `hops` of a named entity (alias-normalized).
/// Errors if the entity is unknown.
pub fn select_by_entity(
    db: &Db,
    scopes: &ScopeSet,
    entity: &str,
    hops: u8,
) -> Result<Vec<NodeRecord>, ComposeError> {
    let anchor = find_existing_entity(db, scopes, entity)?
        .ok_or_else(|| ComposeError::Invalid(format!("unknown entity {entity:?}")))?;
    let sg = db.traverse(&TraversalQuery {
        scopes: scopes.clone(),
        seeds: vec![anchor.id],
        max_hops: hops,
        edge_types: None,
        direction: Direction::Both,
        as_of: None,
    })?;
    Ok(sg
        .nodes
        .into_iter()
        .filter(|n| n.label.as_str() == MEMORY_LABEL)
        .filter(|n| {
            MEMORY_TOMBSTONE_PROPS
                .iter()
                .all(|p| !n.props.contains_key(*p))
        })
        .collect())
}

/// Filesystem-safe slug: unsafe chars → '-', trim, drop trailing dots, cap 100 chars.
pub fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|[]#^".contains(c) {
                '-'
            } else {
                c
            }
        })
        .take(100)
        .collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() {
        "untitled".into()
    } else {
        s
    }
}

fn ystr(s: impl Into<String>) -> Yaml {
    Yaml::String(s.into())
}

fn prop_to_yaml(v: &PropValue) -> Option<Yaml> {
    Some(match v {
        PropValue::Str(s) => ystr(s.clone()),
        PropValue::Int(i) | PropValue::DateTime(i) => Yaml::Number((*i).into()),
        PropValue::Float(f) => Yaml::Number((*f).into()),
        PropValue::Bool(b) => Yaml::Bool(*b),
        PropValue::Bytes(_) => return None, // not representable; omitted
    })
}

pub fn render_memory_note(node: &NodeRecord, entity_names: &[String]) -> Note {
    let mut fm = Mapping::new();
    fm.insert(ystr(TOPODB_ID_KEY), ystr(node.id.to_string()));
    for (k, v) in &node.props {
        if [
            MEMORY_CONTENT_PROP,
            MEMORY_CONTENT_HASH_PROP,
            MEMORY_SUPERSEDED_AT_PROP,
            MEMORY_FORGOTTEN_AT_PROP,
        ]
        .contains(&k.as_str())
        {
            continue;
        }
        if let Some(y) = prop_to_yaml(v) {
            fm.insert(ystr(k.clone()), y);
        }
    }
    if !entity_names.is_empty() {
        fm.insert(
            ystr(RELATED_KEY),
            Yaml::Sequence(
                entity_names
                    .iter()
                    .map(|n| ystr(format!("[[{n}]]")))
                    .collect(),
            ),
        );
    }
    let content = match node.props.get(MEMORY_CONTENT_PROP) {
        Some(PropValue::Str(s)) => s.clone(),
        _ => String::new(),
    };
    Note {
        frontmatter: fm,
        body: format!("{content}\n"),
    }
}

pub fn render_entity_stub(node: &NodeRecord, aliases: &[String]) -> Note {
    let mut fm = Mapping::new();
    fm.insert(ystr(TOPODB_ID_KEY), ystr(node.id.to_string()));
    fm.insert(ystr(crate::ENTITY_STUB_KEY), Yaml::Bool(true));
    if !aliases.is_empty() {
        fm.insert(
            ystr("aliases"),
            Yaml::Sequence(aliases.iter().map(|a| ystr(a.clone())).collect()),
        );
    }
    Note {
        frontmatter: fm,
        body: String::new(),
    }
}

/// Non-clobber write: identical on disk → unchanged; differs & !overwrite →
/// skipped; else write and bump `seeded` (memory notes) or `stubs` (entity
/// stubs) per `kind_seeded`.
fn place(
    path: &std::path::Path,
    note: &Note,
    overwrite: bool,
    report: &mut crate::SeedReport,
    kind_seeded: bool,
) -> Result<(), String> {
    let rendered = note.serialize();
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == rendered => {
            report.unchanged += 1;
            return Ok(());
        }
        Ok(_) if !overwrite => {
            report.skipped += 1;
            return Ok(());
        }
        _ => {}
    }
    crate::write_note(path, note)?;
    if kind_seeded {
        report.seeded += 1
    } else {
        report.stubs += 1
    }
    Ok(())
}

fn title_or_id(node: &NodeRecord) -> String {
    if let Some(PropValue::Str(t)) = node.props.get(TITLE_PROP) {
        let s = slug(t);
        if s != "untitled" {
            return s;
        }
    }
    if let Some(PropValue::Str(c)) = node.props.get(MEMORY_CONTENT_PROP) {
        let first_line = c.lines().next().unwrap_or("");
        let truncated: String = first_line.chars().take(60).collect();
        let s = slug(&truncated);
        if s != "untitled" {
            return s;
        }
    }
    node.id.to_string()
}

/// Resolve a collision-safe filename: `base.md`, or on clash with a
/// different node id, `base-{last6(id)}.md`.
fn resolve_filename(base: String, id: NodeId, used: &mut BTreeMap<String, NodeId>) -> String {
    match used.get(&base) {
        Some(existing) if *existing != id => {
            let id_str = id.to_string();
            let suffix = &id_str[id_str.len().saturating_sub(6)..];
            format!("{base}-{suffix}.md")
        }
        _ => {
            used.insert(base.clone(), id);
            format!("{base}.md")
        }
    }
}

pub fn seed_vault(
    db: &Db,
    scopes: &ScopeSet,
    vault: &std::path::Path,
    memories: &[NodeRecord],
    overwrite: bool,
) -> Result<crate::SeedReport, String> {
    std::fs::create_dir_all(vault).map_err(|e| e.to_string())?;
    let mut report = crate::SeedReport::default();
    let mut used: BTreeMap<String, NodeId> = BTreeMap::new();
    let mut all_entities: BTreeMap<NodeId, (NodeRecord, String)> = BTreeMap::new();

    // Pass 1: gather each memory's linked entities (no writes yet) so entity
    // slugs can be reserved BEFORE memory notes claim any filenames — a stub
    // must always keep the plain `name.md` (wikilink text and filename must
    // agree), so a colliding memory note has to be the one pushed to a suffix.
    let mut per_memory_entities: Vec<Vec<String>> = Vec::with_capacity(memories.len());
    for mem in memories {
        let edges = match db.edges_from(scopes, mem.id, None, None, true) {
            Ok(e) => e,
            Err(e) => {
                report.errors.push(crate::FileError {
                    file: mem.id.to_string(),
                    reason: e.to_string(),
                });
                per_memory_entities.push(Vec::new());
                continue;
            }
        };
        let mut entity_names: Vec<String> = Vec::new();
        for edge in edges {
            let Some(node) = db.node(scopes, edge.to) else {
                continue;
            };
            if node.label.as_str() != ENTITY_LABEL {
                continue;
            }
            let name = match node.props.get(ENTITY_NAME_PROP) {
                Some(PropValue::Str(s)) => s.clone(),
                _ => continue,
            };
            entity_names.push(name.clone());
            all_entities.entry(node.id).or_insert((node, name));
        }
        per_memory_entities.push(entity_names);
    }

    // Pass 2: reserve every entity's slug in `used` before any memory note is
    // named, so stubs keep their plain names on collision.
    for (ent_id, (_, name)) in &all_entities {
        let base = slug(name);
        resolve_filename(base, *ent_id, &mut used);
    }

    // Pass 3: write memory notes, using whatever filename `used` yields —
    // reserved entity slugs already occupy their plain names.
    for (mem, entity_names) in memories.iter().zip(per_memory_entities.iter()) {
        let note = render_memory_note(mem, entity_names);
        let base = title_or_id(mem);
        let filename = resolve_filename(base, mem.id, &mut used);
        let path = vault.join(&filename);
        if let Err(e) = place(&path, &note, overwrite, &mut report, true) {
            let rel = path
                .strip_prefix(vault)
                .unwrap_or(&path)
                .display()
                .to_string();
            report.errors.push(crate::FileError {
                file: rel,
                reason: e,
            });
        }
    }

    for (ent_id, (ent_node, name)) in &all_entities {
        let aliases: Vec<String> =
            match db.edges_to(scopes, *ent_id, None, Some(ALIAS_EDGE_TYPE), true) {
                Ok(edges) => edges
                    .into_iter()
                    .filter_map(|edge| db.node(scopes, edge.from))
                    .filter_map(|n| match n.props.get(ALIAS_NAME_PROP) {
                        Some(PropValue::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                Err(e) => {
                    report.errors.push(crate::FileError {
                        file: name.clone(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
        let note = render_entity_stub(ent_node, &aliases);
        let base = slug(name);
        let filename = resolve_filename(base, *ent_id, &mut used);
        let path = vault.join(&filename);
        if let Err(e) = place(&path, &note, overwrite, &mut report, false) {
            let rel = path
                .strip_prefix(vault)
                .unwrap_or(&path)
                .display()
                .to_string();
            report.errors.push(crate::FileError {
                file: rel,
                reason: e,
            });
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{note_to_input, Note, NoteAction};
    use topodb::Scope;
    use topodb_json::scopes_to_scope_set;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("t.redb"), topodb_json::default_spec()).unwrap();
        (dir, db)
    }
    fn input(src: &str) -> crate::NoteInput {
        note_to_input(&Note::parse(src).unwrap()).unwrap()
    }

    #[test]
    fn slug_sanitizes() {
        assert_eq!(slug("auth: the/plan?"), "auth- the-plan-");
        assert_eq!(slug(""), "untitled");
        assert_eq!(slug(&"x".repeat(300)).len(), 100);
    }

    #[test]
    fn slug_strips_obsidian_wikilink_and_heading_chars() {
        let s = slug("Fact about [[Redis]] done");
        assert!(!s.contains('['));
        assert!(!s.contains(']'));
        let s2 = slug("tag #topic and heading ^block");
        assert!(!s2.contains('#'));
        assert!(!s2.contains('^'));
    }

    #[test]
    fn slug_is_char_boundary_safe() {
        let s = slug(&"中".repeat(34)); // 102 bytes of CJK — must not panic
        assert!(!s.is_empty());
        assert!(s.chars().count() <= 100);
        assert_eq!(slug(&"é".repeat(200)).chars().count(), 100);
    }

    #[test]
    fn seed_writes_notes_stubs_and_respects_existing() {
        let (_d, db) = db();
        let lookup = scopes_to_scope_set(&[Scope::Shared]);
        let out = crate::plan_note(
            &db,
            Scope::Shared,
            &lookup,
            1,
            &input("---\nstatus: open\n---\nGamma uses [[redb]].\n"),
        )
        .unwrap();
        let NoteAction::Created { memory_id } = out.action else {
            panic!()
        };
        db.submit(out.ops).unwrap();
        let mem = db.node(&lookup, memory_id).unwrap();

        let vdir = tempfile::tempdir().unwrap();
        let r = seed_vault(&db, &lookup, vdir.path(), std::slice::from_ref(&mem), false).unwrap();
        assert_eq!((r.seeded, r.stubs, r.skipped, r.unchanged), (1, 1, 0, 0));

        // Files: memory note named from first content line; stub named for entity.
        let names: Vec<_> = std::fs::read_dir(vdir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().any(|n| n == "redb.md"));
        let note_name = names
            .iter()
            .find(|n| n.starts_with("Gamma"))
            .unwrap()
            .clone();
        let text = std::fs::read_to_string(vdir.path().join(&note_name)).unwrap();
        assert!(text.contains(&format!("topodb-id: {memory_id}")));
        assert!(text.contains("status: open"));
        assert!(text.contains("- \"[[redb]]\"") || text.contains("- '[[redb]]'"));
        assert!(text.ends_with("Gamma uses [[redb]].\n"));
        assert!(!text.contains("content_hash"));

        // Re-seed: everything unchanged.
        let r2 = seed_vault(&db, &lookup, vdir.path(), std::slice::from_ref(&mem), false).unwrap();
        assert_eq!((r2.seeded + r2.stubs, r2.unchanged), (0, 2));

        // Local edit is protected; --overwrite clobbers.
        std::fs::write(vdir.path().join(&note_name), "local edit").unwrap();
        let r3 = seed_vault(&db, &lookup, vdir.path(), std::slice::from_ref(&mem), false).unwrap();
        assert_eq!((r3.skipped, r3.seeded), (1, 0));
        let r4 = seed_vault(&db, &lookup, vdir.path(), &[mem], true).unwrap();
        assert_eq!(r4.seeded, 1);
    }

    #[test]
    fn stub_wins_plain_name_on_filename_collision_with_memory() {
        let (_d, db) = db();
        let lookup = scopes_to_scope_set(&[Scope::Shared]);
        // Memory's derived filename ("redis.md", from its title) collides
        // with the linked entity's own slug ("redis.md"). The stub must win
        // the plain name so wikilink text ("[[redis]]") and filename agree;
        // the memory note gets bumped to a suffixed name instead.
        let out = crate::plan_note(
            &db,
            Scope::Shared,
            &lookup,
            1,
            &input("---\ntitle: redis\n---\nFact about [[redis]].\n"),
        )
        .unwrap();
        let NoteAction::Created { memory_id } = out.action else {
            panic!()
        };
        db.submit(out.ops).unwrap();
        let mem = db.node(&lookup, memory_id).unwrap();

        let vdir = tempfile::tempdir().unwrap();
        let r = seed_vault(&db, &lookup, vdir.path(), std::slice::from_ref(&mem), false).unwrap();
        assert_eq!((r.seeded, r.stubs), (1, 1));

        let names: Vec<_> = std::fs::read_dir(vdir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().any(|n| n == "redis.md"), "{names:?}");
        let stub_text = std::fs::read_to_string(vdir.path().join("redis.md")).unwrap();
        assert!(stub_text.contains("entity: true"));

        let memory_name = names
            .iter()
            .find(|n| *n != "redis.md")
            .unwrap_or_else(|| panic!("expected a suffixed memory note among {names:?}"));
        assert!(memory_name.starts_with("redis-"), "{names:?}");
        let memory_text = std::fs::read_to_string(vdir.path().join(memory_name)).unwrap();
        assert!(memory_text.contains(&format!("topodb-id: {memory_id}")));

        // Re-seed is stable: same two files, nothing re-churned.
        let r2 = seed_vault(&db, &lookup, vdir.path(), std::slice::from_ref(&mem), false).unwrap();
        assert_eq!((r2.seeded + r2.stubs, r2.unchanged), (0, 2));
    }

    #[test]
    fn select_by_query_finds_live_memories_only() {
        let (_d, db) = db();
        let lookup = scopes_to_scope_set(&[Scope::Shared]);
        for txt in ["kafka handles the event stream", "postgres stores billing"] {
            let o = crate::plan_note(
                &db,
                Scope::Shared,
                &lookup,
                1,
                &input(&format!("{txt} [[infra]]\n")),
            )
            .unwrap();
            db.submit(o.ops).unwrap();
        }
        let hits = select_by_query(&db, &lookup, "event stream", 5, None).unwrap();
        assert!(hits.iter().all(|n| n.label.as_str() == "Memory"));
        assert!(hits.iter().any(|n| matches!(n.props.get("content"),
            Some(topodb::PropValue::Str(s)) if s.contains("kafka"))));

        // Tombstoned memories are excluded from BOTH selectors.
        let kafka = hits
            .iter()
            .find(|n| {
                matches!(n.props.get("content"),
                Some(topodb::PropValue::Str(s)) if s.contains("kafka"))
            })
            .unwrap();
        let (ops, _) =
            topodb_json::plan_forget(&db, Scope::Shared, &[kafka.id.to_string()], 9).unwrap();
        db.submit(ops).unwrap();
        let hits2 = select_by_query(&db, &lookup, "event stream", 5, None).unwrap();
        assert!(hits2.iter().all(|n| n.id != kafka.id));
        let by_ent = select_by_entity(&db, &lookup, "infra", 2).unwrap();
        assert!(by_ent.iter().all(|n| n.id != kafka.id));
    }

    #[test]
    fn select_by_entity_traverses_and_rejects_unknown() {
        let (_d, db) = db();
        let lookup = scopes_to_scope_set(&[Scope::Shared]);
        let o = crate::plan_note(
            &db,
            Scope::Shared,
            &lookup,
            1,
            &input("Zeta fact about [[Widget]].\n"),
        )
        .unwrap();
        db.submit(o.ops).unwrap();
        let hits = select_by_entity(&db, &lookup, "widget", 2).unwrap(); // alias-normalized name
        assert_eq!(hits.len(), 1);
        assert!(matches!(
            select_by_entity(&db, &lookup, "nope", 2),
            Err(topodb_json::ComposeError::Invalid(_))
        ));
    }
}
