//! Seed — graph → OKF bundle. Each selected concept memory renders one `.md`
//! page: frontmatter rebuilt from the entity's props + promoted provenance
//! edges (nested `generated`/`verified`/`sources`, `tags` re-split to a list,
//! dotted-key props un-flattened), body = the memory content. Reserved
//! `index.md` (root carries `okf_version`) and `log.md` are generated
//! deterministically. Writes are non-clobbering unless `overwrite`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::index::render_root_index;
use crate::log::render_log;
use crate::mapping::{prop_to_yaml, set_nested};
use crate::{
    Note, ABOUT_EDGE, AT_PROP, AUTHORED_BY_EDGE, AUTHOR_KEY, BY_KEY, ENTITY_KIND_PROP,
    GENERATED_BY_EDGE, GENERATED_KEY, PATH_PROP, RESOURCE_KEY, SOURCED_FROM_EDGE, SOURCES_KEY,
    TAGS_KEY, TITLE_KEY, TOPODB_ID_KEY, TYPE_PROP, VERIFIED_BY_EDGE, VERIFIED_KEY,
};
use serde_yaml::{Mapping, Value as Yaml};
use topodb::{Db, NodeId, NodeRecord, PropValue, ScopeSet, TimeAxis};
use topodb_json::{ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP};

/// Whether a seeded reserved file counts as a page or a generated file.
enum Kind {
    Page,
    Reserved,
}

pub fn seed_okf(
    db: &Db,
    scopes: &ScopeSet,
    memories: &[NodeRecord],
    bundle: &Path,
    with_log: bool,
    overwrite: bool,
) -> Result<crate::SeedReport, String> {
    std::fs::create_dir_all(bundle).map_err(|e| e.to_string())?;
    let mut report = crate::SeedReport::default();
    // (rel path, description) of every emitted page — feeds the root index.
    let mut emitted: Vec<(String, String)> = Vec::new();
    let mut used_paths: BTreeMap<String, ()> = BTreeMap::new();

    for mem in memories {
        let Some(entity) = concept_entity(db, scopes, mem) else {
            continue;
        };
        let rel = page_path(&entity, &mut used_paths);
        let note = render_concept(db, scopes, &entity, mem);
        let description = entity
            .props
            .get("description")
            .and_then(|v| match v {
                PropValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        emitted.push((rel.clone(), description));
        place(
            &bundle.join(&rel),
            &note,
            overwrite,
            &mut report,
            Kind::Page,
        );
    }

    emitted.sort();
    emitted.dedup();
    let index = render_root_index(&emitted);
    place(
        &bundle.join("index.md"),
        &index,
        overwrite,
        &mut report,
        Kind::Reserved,
    );

    if with_log {
        let log = render_log(&emitted);
        place(
            &bundle.join("log.md"),
            &log,
            overwrite,
            &mut report,
            Kind::Reserved,
        );
    }

    Ok(report)
}

/// The concept entity a memory is attached to (its open `about` target).
fn concept_entity(db: &Db, scopes: &ScopeSet, mem: &NodeRecord) -> Option<NodeRecord> {
    db.edges_from(
        scopes,
        mem.id,
        None,
        Some(ABOUT_EDGE),
        true,
        TimeAxis::Valid,
    )
    .ok()?
    .into_iter()
    .filter_map(|e| db.node(scopes, e.to))
    .find(|n| n.label.as_str() == ENTITY_LABEL)
}

/// The bundle-relative path for a concept: its stored `path` prop, else
/// `<slug(type)>/<slug(title)>.md`. On a rare collision, a `-N` suffix.
fn page_path(entity: &NodeRecord, used: &mut BTreeMap<String, ()>) -> String {
    let base = match entity.props.get(PATH_PROP) {
        Some(PropValue::Str(p)) => p.clone(),
        _ => {
            let ty = str_prop(entity, TYPE_PROP).unwrap_or_else(|| "concept".into());
            let title = str_prop(entity, ENTITY_NAME_PROP).unwrap_or_else(|| entity.id.to_string());
            format!("{}/{}.md", slug(&ty), slug(&title))
        }
    };
    if used.insert(base.clone(), ()).is_none() {
        return base;
    }
    let (stem, ext) = base.rsplit_once('.').unwrap_or((base.as_str(), "md"));
    for n in 2.. {
        let candidate = format!("{stem}-{n}.{ext}");
        if used.insert(candidate.clone(), ()).is_none() {
            return candidate;
        }
    }
    base
}

fn render_concept(db: &Db, scopes: &ScopeSet, entity: &NodeRecord, mem: &NodeRecord) -> Note {
    let mut fm = Mapping::new();
    fm.insert(ystr(TOPODB_ID_KEY), ystr(entity.id.to_string()));

    for key in [TYPE_PROP, TITLE_KEY, "description", RESOURCE_KEY] {
        if let Some(v) = entity.props.get(key) {
            fm.insert(ystr(key), prop_to_yaml(v));
        }
    }
    if let Some(PropValue::Str(s)) = entity.props.get(TAGS_KEY) {
        let seq: Vec<Yaml> = s.split(", ").filter(|t| !t.is_empty()).map(ystr).collect();
        if !seq.is_empty() {
            fm.insert(ystr(TAGS_KEY), Yaml::Sequence(seq));
        }
    }
    for key in ["status", "stale_after"] {
        if let Some(v) = entity.props.get(key) {
            fm.insert(ystr(key), prop_to_yaml(v));
        }
    }

    // Long-tail unknown props (incl. dotted keys) → un-flattened nested maps.
    let mut nested = Mapping::new();
    for (k, v) in &entity.props {
        if is_reserved_prop(k) {
            continue;
        }
        let segs: Vec<&str> = k.split('.').collect();
        set_nested(&mut nested, &segs, prop_to_yaml(v));
    }
    for (k, v) in nested {
        fm.insert(k, v);
    }

    // Promoted provenance rebuilt from the concept's outgoing edges.
    let out = db
        .edges_from(scopes, entity.id, None, None, true, TimeAxis::Valid)
        .unwrap_or_default();

    if let Some(e) = out.iter().find(|e| e.ty.as_str() == GENERATED_BY_EDGE) {
        if let Some(by) = entity_name(db, scopes, e.to) {
            let mut m = Mapping::new();
            m.insert(ystr(BY_KEY), ystr(by));
            if let Some(PropValue::Str(at)) = e.props.get(AT_PROP) {
                m.insert(ystr(crate::AT_KEY), ystr(at.clone()));
            }
            fm.insert(ystr(GENERATED_KEY), Yaml::Mapping(m));
        }
    }

    let mut verified: Vec<(String, Mapping)> = Vec::new();
    for e in out.iter().filter(|e| e.ty.as_str() == VERIFIED_BY_EDGE) {
        if let Some(by) = entity_name(db, scopes, e.to) {
            let mut m = Mapping::new();
            m.insert(ystr(BY_KEY), ystr(by.clone()));
            let mut key = by;
            if let Some(PropValue::Str(at)) = e.props.get(AT_PROP) {
                m.insert(ystr(crate::AT_KEY), ystr(at.clone()));
                key.push_str(at);
            }
            verified.push((key, m));
        }
    }
    if !verified.is_empty() {
        verified.sort_by(|a, b| a.0.cmp(&b.0));
        fm.insert(
            ystr(VERIFIED_KEY),
            Yaml::Sequence(
                verified
                    .into_iter()
                    .map(|(_, m)| Yaml::Mapping(m))
                    .collect(),
            ),
        );
    }

    let mut sources: Vec<(String, Mapping)> = Vec::new();
    for e in out.iter().filter(|e| e.ty.as_str() == SOURCED_FROM_EDGE) {
        let Some(resource) = entity_name(db, scopes, e.to) else {
            continue;
        };
        let mut m = Mapping::new();
        m.insert(ystr(RESOURCE_KEY), ystr(resource.clone()));
        for (k, v) in &e.props {
            m.insert(ystr(k.clone()), prop_to_yaml(v));
        }
        // author rebuilt from the source entity's authored_by edge.
        if let Ok(src_out) = db.edges_from(
            scopes,
            e.to,
            None,
            Some(AUTHORED_BY_EDGE),
            true,
            TimeAxis::Valid,
        ) {
            if let Some(ae) = src_out.first() {
                if let Some(author) = entity_name(db, scopes, ae.to) {
                    m.insert(ystr(AUTHOR_KEY), ystr(author));
                }
            }
        }
        sources.push((resource, m));
    }
    if !sources.is_empty() {
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        fm.insert(
            ystr(SOURCES_KEY),
            Yaml::Sequence(sources.into_iter().map(|(_, m)| Yaml::Mapping(m)).collect()),
        );
    }

    let content = match mem.props.get(MEMORY_CONTENT_PROP) {
        Some(PropValue::Str(s)) => s.clone(),
        _ => String::new(),
    };
    Note {
        frontmatter: fm,
        body: format!("{content}\n"),
    }
}

/// Entity props handled explicitly (or internal to TopoDB) and therefore not
/// emitted through the long-tail un-flatten path.
fn is_reserved_prop(key: &str) -> bool {
    matches!(
        key,
        ENTITY_NAME_PROP
            | PATH_PROP
            | ENTITY_KIND_PROP
            | TYPE_PROP
            | TITLE_KEY
            | "description"
            | RESOURCE_KEY
            | "status"
            | "stale_after"
            | TAGS_KEY
    )
}

fn entity_name(db: &Db, scopes: &ScopeSet, id: NodeId) -> Option<String> {
    db.node(scopes, id)
        .and_then(|n| match n.props.get(ENTITY_NAME_PROP) {
            Some(PropValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
}

fn str_prop(entity: &NodeRecord, key: &str) -> Option<String> {
    match entity.props.get(key) {
        Some(PropValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Non-clobber write: identical on disk → unchanged; differs & !overwrite →
/// skipped; else write and count.
fn place(path: &Path, note: &Note, overwrite: bool, report: &mut crate::SeedReport, kind: Kind) {
    let rendered = note.serialize();
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == rendered => {
            report.unchanged += 1;
            return;
        }
        Ok(_) if !overwrite => {
            report.skipped += 1;
            return;
        }
        _ => {}
    }
    if let Err(e) = crate::vault::write_note(path, note) {
        report.errors.push(crate::FileError {
            file: path.display().to_string(),
            reason: e,
        });
        return;
    }
    match kind {
        Kind::Page => report.seeded += 1,
        Kind::Reserved => report.reserved += 1,
    }
}

fn ystr(s: impl Into<String>) -> Yaml {
    Yaml::String(s.into())
}

/// Filesystem-safe slug (mirrors `topodb-obsidian::slug`): unsafe chars → '-',
/// trimmed, trailing dots dropped, capped at 100 chars.
fn slug(name: &str) -> String {
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
