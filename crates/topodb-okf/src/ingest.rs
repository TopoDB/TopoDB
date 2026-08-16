//! Ingest — OKF bundle → graph. Each concept page becomes an Entity (with a
//! bundle-relative `path` prop) + one attached `about` Memory, plus body-link
//! `references` edges and promoted provenance (actor/source entities). Broken
//! links become dangling stubs, never errors (OKF §11 tolerant consumer).
//!
//! Writes go through `Op`s and one `db.submit` per page, mirroring
//! `topodb-obsidian`'s per-note flow. `topodb-id` (the concept entity id) is
//! stamped back into each file as the identity anchor for idempotent re-ingest.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::mapping::{flatten_into, yaml_scalar_to_prop};
use crate::{
    links, Note, ABOUT_EDGE, AT_KEY, AT_PROP, AUTHORED_BY_EDGE, AUTHOR_KEY, BY_KEY,
    ENTITY_KIND_PROP, GENERATED_BY_EDGE, GENERATED_KEY, KIND_ACTOR, KIND_SOURCE, PATH_PROP,
    RESOURCE_KEY, SOURCED_FROM_EDGE, SOURCES_KEY, TAGS_KEY, TITLE_KEY, TYPE_PROP, VERIFIED_BY_EDGE,
    VERIFIED_KEY,
};
use topodb::{Db, EdgeId, NodeId, NodeRecord, Op, PropValue, Props, Scope, ScopeSet, TimeAxis};
use topodb_json::{
    entity_dedup_key, find_existing_entity, memory_props, normalize_content, plan_supersede,
    ComposeError, ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP, MEMORY_FORGOTTEN_AT_PROP,
    MEMORY_LABEL, MEMORY_SUPERSEDED_AT_PROP,
};

/// Optional embedding hook: text → (model_name, vector). MCP passes its
/// embedder; the CLI passes `None`. The engine stays policy-free.
pub type EmbedFn<'a> = &'a dyn Fn(&str) -> Option<(String, Vec<f32>)>;

/// One `sources[]` entry: the resource (source entity name), the sourced-from
/// edge's scalar props (id/title/last_modified/usage_count), and an optional
/// author actor.
struct SourceSpec {
    resource: String,
    edge_props: Props,
    author: Option<String>,
}

/// Parsed concept frontmatter + body, ready to plan into ops.
struct ConceptDoc {
    name: String,
    /// Entity props excluding `name`/`path` (type/title/description/… + flattened
    /// dotted-key long tail).
    props: Props,
    generated: Option<(String, Option<String>)>,
    verified: Vec<(String, Option<String>)>,
    sources: Vec<SourceSpec>,
    /// Resolved bundle-relative link targets (body `references` edges).
    links: Vec<String>,
    /// Trimmed body = the memory content.
    body: String,
}

enum Outcome {
    Created,
    Superseded,
    Skipped,
}

struct ConceptPlan {
    outcome: Outcome,
    entity_id: NodeId,
    ops: Vec<Op>,
    new_memory: Option<(NodeId, String)>,
    new_entities: Vec<(NodeId, String)>,
}

pub fn ingest_okf(
    db: &Db,
    bundle: &Path,
    write_scope: Scope,
    lookup: &ScopeSet,
    now_ms: i64,
    dry_run: bool,
    embed: Option<EmbedFn>,
) -> Result<crate::IngestReport, String> {
    let mut report = crate::IngestReport::default();
    for path in crate::walk_bundle(bundle)? {
        let rel = rel_path(bundle, &path);
        let fail = |reason: String, report: &mut crate::IngestReport| {
            report.errors.push(crate::FileError {
                file: rel.clone(),
                reason,
            });
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                fail(e.to_string(), &mut report);
                continue;
            }
        };
        let mut note = match Note::parse(&text) {
            Ok(n) => n,
            Err(e) => {
                fail(e, &mut report);
                continue;
            }
        };
        let plan = match plan_concept(db, write_scope, lookup, &rel, &note, now_ms) {
            Ok(p) => p,
            Err(ComposeError::Invalid(m)) => {
                fail(m, &mut report);
                continue;
            }
            Err(ComposeError::Engine(e)) => {
                fail(e.to_string(), &mut report);
                continue;
            }
        };

        match plan.outcome {
            Outcome::Created => report.ingested += 1,
            Outcome::Superseded => report.superseded += 1,
            Outcome::Skipped => {
                report.skipped += 1;
                continue;
            }
        }
        if dry_run {
            continue;
        }

        let mut ops = plan.ops;
        if let Some(embed) = embed {
            if let Some((id, content)) = &plan.new_memory {
                if let Some((model, vector)) = embed(content) {
                    ops.push(Op::SetEmbedding {
                        id: *id,
                        model,
                        vector,
                    });
                }
            }
            for (id, name) in &plan.new_entities {
                if let Some((model, vector)) = embed(name) {
                    ops.push(Op::SetEmbedding {
                        id: *id,
                        model,
                        vector,
                    });
                }
            }
        }
        if !ops.is_empty() {
            if let Err(e) = db.submit(ops) {
                fail(e.to_string(), &mut report);
                continue;
            }
        }
        let id_str = plan.entity_id.to_string();
        if note.id().as_deref() != Some(id_str.as_str()) {
            note.set_id(&id_str);
            if let Err(e) = crate::vault::write_note(&path, &note) {
                fail(format!("db updated but id stamp failed: {e}"), &mut report);
            }
        }
    }
    Ok(report)
}

/// Bundle-relative, forward-slash path for `path` under `bundle`.
fn rel_path(bundle: &Path, path: &Path) -> String {
    path.strip_prefix(bundle)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn plan_concept(
    db: &Db,
    scope: Scope,
    lookup: &ScopeSet,
    rel: &str,
    note: &Note,
    now_ms: i64,
) -> Result<ConceptPlan, ComposeError> {
    let doc = parse_concept(note, rel);

    // Resolve the concept entity: stamped topodb-id (an Entity) first, else the
    // path prop, else a fresh node.
    let existing = resolve_entity(db, lookup, note, rel);
    let (entity_id, existing_node, is_new) = match existing {
        Some(node) => (node.id, Some(node), false),
        None => (NodeId::new(), None, true),
    };

    let live_memory = existing_node
        .as_ref()
        .and_then(|n| attached_memory(db, lookup, n.id));
    if let Some(mem) = &live_memory {
        let stored = mem
            .props
            .get(MEMORY_CONTENT_PROP)
            .and_then(|v| match v {
                PropValue::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("");
        if normalize_content(stored) == normalize_content(&doc.body) {
            return Ok(ConceptPlan {
                outcome: Outcome::Skipped,
                entity_id,
                ops: Vec::new(),
                new_memory: None,
                new_entities: Vec::new(),
            });
        }
    }

    let mut planner = Planner::new(scope);

    // Concept entity: create fresh, or set props on an existing stub/concept.
    let mut entity_props = doc.props.clone();
    entity_props.insert(ENTITY_NAME_PROP.into(), PropValue::Str(doc.name.clone()));
    entity_props.insert(PATH_PROP.into(), PropValue::Str(rel.to_string()));
    if is_new {
        planner.ops.push(Op::CreateNode {
            id: entity_id,
            scope,
            label: ENTITY_LABEL.into(),
            props: entity_props,
        });
        planner.new_entities.push((entity_id, doc.name.clone()));
    } else {
        let set: std::collections::BTreeMap<String, Option<PropValue>> = entity_props
            .into_iter()
            .map(|(k, v)| (k, Some(v)))
            .collect();
        planner.ops.push(Op::SetNodeProps {
            id: entity_id,
            props: set,
        });
    }

    // Memory: supersede the old one if the body changed, then attach a new one.
    let outcome = if let Some(old) = &live_memory {
        let (sup_ops, _) = plan_supersede(db, scope, &[old.id.to_string()], now_ms)?;
        planner.ops.extend(sup_ops);
        Outcome::Superseded
    } else {
        Outcome::Created
    };
    let memory_id = NodeId::new();
    let mprops = memory_props(&doc.body, None).map_err(ComposeError::Invalid)?;
    planner.ops.push(Op::CreateNode {
        id: memory_id,
        scope,
        label: MEMORY_LABEL.into(),
        props: mprops,
    });
    planner.create_edge(db, lookup, ABOUT_EDGE, memory_id, entity_id, Props::new());

    // Body links → references edges (broken links create dangling stubs).
    for target in &doc.links {
        let tid = planner.path_entity(db, lookup, target);
        planner.create_edge(
            db,
            lookup,
            crate::REFERENCES_EDGE,
            entity_id,
            tid,
            Props::new(),
        );
    }

    // Promoted provenance.
    if let Some((by, at)) = &doc.generated {
        let actor = planner.named_entity(db, lookup, by, KIND_ACTOR);
        planner.create_edge(
            db,
            lookup,
            GENERATED_BY_EDGE,
            entity_id,
            actor,
            at_props(at),
        );
    }
    for (by, at) in &doc.verified {
        let actor = planner.named_entity(db, lookup, by, KIND_ACTOR);
        planner.create_edge(db, lookup, VERIFIED_BY_EDGE, entity_id, actor, at_props(at));
    }
    for src in &doc.sources {
        let source = planner.named_entity(db, lookup, &src.resource, KIND_SOURCE);
        planner.create_edge(
            db,
            lookup,
            SOURCED_FROM_EDGE,
            entity_id,
            source,
            src.edge_props.clone(),
        );
        if let Some(author) = &src.author {
            let actor = planner.named_entity(db, lookup, author, KIND_ACTOR);
            planner.create_edge(db, lookup, AUTHORED_BY_EDGE, source, actor, Props::new());
        }
    }

    Ok(ConceptPlan {
        outcome,
        entity_id,
        ops: planner.ops,
        new_memory: Some((memory_id, doc.body)),
        new_entities: planner.new_entities,
    })
}

fn at_props(at: &Option<String>) -> Props {
    let mut p = Props::new();
    if let Some(at) = at {
        p.insert(AT_PROP.into(), PropValue::Str(at.clone()));
    }
    p
}

/// Accumulates a single page's ops with in-batch dedup of newly-minted
/// provenance entities, path stubs, and edges.
struct Planner {
    scope: Scope,
    ops: Vec<Op>,
    named: HashMap<String, NodeId>,
    paths: HashMap<String, NodeId>,
    edges: HashSet<(NodeId, String, NodeId)>,
    new_entities: Vec<(NodeId, String)>,
}

impl Planner {
    fn new(scope: Scope) -> Self {
        Planner {
            scope,
            ops: Vec::new(),
            named: HashMap::new(),
            paths: HashMap::new(),
            edges: HashSet::new(),
            new_entities: Vec::new(),
        }
    }

    /// Find-or-create an entity by name (actors/sources), setting its `kind`.
    fn named_entity(&mut self, db: &Db, lookup: &ScopeSet, name: &str, kind: &str) -> NodeId {
        let key = entity_dedup_key(name);
        if let Some(id) = self.named.get(&key) {
            return *id;
        }
        if let Ok(Some(node)) = find_existing_entity(db, lookup, name) {
            self.named.insert(key, node.id);
            return node.id;
        }
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert(ENTITY_NAME_PROP.into(), PropValue::Str(name.to_string()));
        props.insert(ENTITY_KIND_PROP.into(), PropValue::Str(kind.to_string()));
        self.ops.push(Op::CreateNode {
            id,
            scope: self.scope,
            label: ENTITY_LABEL.into(),
            props,
        });
        self.named.insert(key, id);
        self.new_entities.push((id, name.to_string()));
        id
    }

    /// Find-or-create an entity by its bundle-relative `path`. A miss creates a
    /// dangling stub (path prop only, no memory) — a broken link is legal.
    fn path_entity(&mut self, db: &Db, lookup: &ScopeSet, rel: &str) -> NodeId {
        if let Some(id) = self.paths.get(rel) {
            return *id;
        }
        if let Ok(hits) =
            db.nodes_by_prop(lookup, ENTITY_LABEL, PATH_PROP, &PropValue::Str(rel.into()))
        {
            if let Some(node) = hits.into_iter().min_by_key(|n| n.id) {
                self.paths.insert(rel.to_string(), node.id);
                return node.id;
            }
        }
        let id = NodeId::new();
        let mut props = Props::new();
        props.insert(PATH_PROP.into(), PropValue::Str(rel.to_string()));
        self.ops.push(Op::CreateNode {
            id,
            scope: self.scope,
            label: ENTITY_LABEL.into(),
            props,
        });
        self.paths.insert(rel.to_string(), id);
        id
    }

    fn create_edge(
        &mut self,
        db: &Db,
        lookup: &ScopeSet,
        ty: &str,
        from: NodeId,
        to: NodeId,
        props: Props,
    ) {
        let key = (from, ty.to_string(), to);
        if self.edges.contains(&key) {
            return;
        }
        self.edges.insert(key);
        if edge_exists(db, lookup, from, ty, to) {
            return;
        }
        self.ops.push(Op::CreateEdge {
            id: EdgeId::new(),
            scope: self.scope,
            ty: ty.into(),
            from,
            to,
            props,
            valid_from: None,
            recorded_at: None,
        });
    }
}

fn edge_exists(db: &Db, lookup: &ScopeSet, from: NodeId, ty: &str, to: NodeId) -> bool {
    db.edges_from(lookup, from, Some(to), Some(ty), true, TimeAxis::Valid)
        .map(|es| !es.is_empty())
        .unwrap_or(false)
}

/// The live (non-tombstoned) Memory attached to `entity` via an open `about`
/// edge, if any.
fn attached_memory(db: &Db, lookup: &ScopeSet, entity: NodeId) -> Option<NodeRecord> {
    db.edges_to(
        lookup,
        entity,
        None,
        Some(ABOUT_EDGE),
        true,
        TimeAxis::Valid,
    )
    .ok()?
    .into_iter()
    .filter_map(|e| db.node(lookup, e.from))
    .find(|n| {
        n.label.as_str() == MEMORY_LABEL
            && !n.props.contains_key(MEMORY_SUPERSEDED_AT_PROP)
            && !n.props.contains_key(MEMORY_FORGOTTEN_AT_PROP)
    })
}

fn resolve_entity(db: &Db, lookup: &ScopeSet, note: &Note, rel: &str) -> Option<NodeRecord> {
    if let Some(raw) = note.id() {
        if let Ok(id) = raw.parse::<NodeId>() {
            if let Some(node) = db.node(lookup, id) {
                if node.label.as_str() == ENTITY_LABEL {
                    return Some(node);
                }
            }
        }
    }
    db.nodes_by_prop(lookup, ENTITY_LABEL, PATH_PROP, &PropValue::Str(rel.into()))
        .ok()?
        .into_iter()
        .min_by_key(|n| n.id)
}

/// Parse a concept page's frontmatter + body into entity props and promotions.
/// Tolerant: unknown keys are flattened, non-string keys ignored, missing
/// optionals accepted (OKF §11).
fn parse_concept(note: &Note, rel: &str) -> ConceptDoc {
    let mut props = Props::new();
    let mut title: Option<String> = None;
    let mut generated = None;
    let mut verified = Vec::new();
    let mut sources = Vec::new();
    let mut flat: Vec<(String, PropValue)> = Vec::new();

    for (k, v) in &note.frontmatter {
        let Some(key) = k.as_str() else { continue };
        match key {
            crate::TOPODB_ID_KEY => {}
            TYPE_PROP | "description" | RESOURCE_KEY | "status" | "stale_after" => {
                if let Some(p) = yaml_scalar_to_prop(v) {
                    props.insert(key.to_string(), p);
                }
            }
            TITLE_KEY => {
                if let Some(s) = v.as_str() {
                    title = Some(s.to_string());
                    props.insert(TITLE_KEY.to_string(), PropValue::Str(s.to_string()));
                }
            }
            TAGS_KEY => {
                props.insert(TAGS_KEY.to_string(), PropValue::Str(join_tags(v)));
            }
            GENERATED_KEY => generated = v.as_mapping().and_then(parse_actor_at),
            VERIFIED_KEY => verified = parse_verified(v),
            SOURCES_KEY => sources = parse_sources(v),
            _ => flatten_into(key, v, &mut flat),
        }
    }
    for (k, v) in flat {
        props.insert(k, v);
    }

    let name = title
        .or_else(|| first_h1(&note.body))
        .unwrap_or_else(|| file_stem(rel));

    let links = resolve_body_links(&note.body, rel);

    ConceptDoc {
        name,
        props,
        generated,
        verified,
        sources,
        links,
        body: note.body.trim_end().to_string(),
    }
}

fn join_tags(v: &serde_yaml::Value) -> String {
    use serde_yaml::Value as Yaml;
    match v {
        Yaml::Sequence(seq) => seq
            .iter()
            .filter_map(|item| match item {
                Yaml::String(s) => Some(s.clone()),
                Yaml::Number(n) => Some(n.to_string()),
                Yaml::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(", "),
        Yaml::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn parse_actor_at(m: &serde_yaml::Mapping) -> Option<(String, Option<String>)> {
    let by = m.get(BY_KEY).and_then(|v| v.as_str())?.to_string();
    let at = m.get(AT_KEY).and_then(|v| v.as_str()).map(str::to_string);
    Some((by, at))
}

fn parse_verified(v: &serde_yaml::Value) -> Vec<(String, Option<String>)> {
    use serde_yaml::Value as Yaml;
    match v {
        Yaml::Sequence(seq) => seq
            .iter()
            .filter_map(|item| item.as_mapping().and_then(parse_actor_at))
            .collect(),
        // A bare mapping is treated as a one-element list (OKF §5.2).
        Yaml::Mapping(m) => parse_actor_at(m).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn parse_sources(v: &serde_yaml::Value) -> Vec<SourceSpec> {
    use serde_yaml::Value as Yaml;
    let maps: Vec<&serde_yaml::Mapping> = match v {
        Yaml::Sequence(seq) => seq.iter().filter_map(|i| i.as_mapping()).collect(),
        Yaml::Mapping(m) => vec![m],
        _ => Vec::new(),
    };
    maps.into_iter()
        .filter_map(|m| {
            let resource = m.get(RESOURCE_KEY).and_then(|v| v.as_str())?.to_string();
            let author = m
                .get(AUTHOR_KEY)
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let mut edge_props = Props::new();
            for (k, val) in m {
                let Some(key) = k.as_str() else { continue };
                if key == RESOURCE_KEY || key == AUTHOR_KEY {
                    continue;
                }
                if let Some(p) = yaml_scalar_to_prop(val) {
                    edge_props.insert(key.to_string(), p);
                }
            }
            Some(SourceSpec {
                resource,
                edge_props,
                author,
            })
        })
        .collect()
}

fn resolve_body_links(body: &str, rel: &str) -> Vec<String> {
    let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (_, href) in links::extract_links(body) {
        if let Some(target) = links::resolve_link(dir, &href) {
            if target != rel && seen.insert(target.clone()) {
                out.push(target);
            }
        }
    }
    out
}

fn first_h1(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        l.trim_start()
            .strip_prefix("# ")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn file_stem(rel: &str) -> String {
    Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string())
}
