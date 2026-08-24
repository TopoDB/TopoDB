//! Derive (spec §7): segments -> Artifact/Chunk nodes + has_chunk/evidence edges.
use crate::chunk::chunk_artifact;
use crate::event::{ArtifactType, Event, Kind};
use crate::ids::{artifact_node_id, chunk_node_id, evidence_edge_id, has_chunk_edge_id};
use crate::lineage::{evidence_windows, EVIDENCE_RULE};
use crate::{Warehouse, WarehouseError};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use topodb::{Db, EdgeRecord, NodeId, Op, PropValue, Props, Scope, ScopeSet, TimeAxis};

pub const DERIVE_VERSION: u32 = 1;
pub const ARTIFACT_LABEL: &str = topodb_json::ARTIFACT_LABEL;
pub const CHUNK_LABEL: &str = topodb_json::CHUNK_LABEL;
pub const HAS_CHUNK_EDGE: &str = "has_chunk";
pub const EVIDENCE_EDGE: &str = "evidence";
pub const DERIVED_BY_PROP: &str = "derived_by";
pub const TIER_PROP: &str = "tier";
pub const PREVIEW_CHARS: usize = 120;

pub type EmbedFn<'a> = &'a dyn Fn(&str) -> Option<(String, Vec<f32>)>;

pub fn derived_by_stamp(model: Option<&str>) -> String {
    format!("warehouse/{DERIVE_VERSION}/{}", model.unwrap_or("none"))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeriveReport {
    pub artifacts_created: usize,
    pub artifacts_updated: usize,
    pub chunks_created: usize,
    pub evidence_edges: usize,
    pub pointer_only: usize,
    pub removed: usize,
    pub markers_skipped: usize,
    pub cross_scope_skipped: usize,
    pub embed_model: Option<String>,
}

fn s(v: &str) -> PropValue {
    PropValue::Str(v.to_string())
}
fn kind_str(k: &ArtifactType) -> &'static str {
    match k {
        ArtifactType::FileRead => "file_read",
        ArtifactType::Command => "command",
        ArtifactType::Diff => "diff",
        ArtifactType::ToolOutput => "tool_output",
        ArtifactType::TranscriptRef => "transcript_ref",
    }
}
fn parse_scope(label: &str) -> Scope {
    topodb_json::resolve_scope(Some(label), Scope::Shared).unwrap_or(Scope::Shared)
}
fn scope_set_for(scope: Scope) -> ScopeSet {
    topodb_json::scopes_to_scope_set(&[scope, Scope::Shared])
}

struct Group {
    scope: String,
    hash: String,
    kind: ArtifactType,
    locator: String,
    bytes: u64,
    redacted: bool,
    first_seen: i64,
    last_seen: i64,
    seen: i64,
    text: Option<String>,
}

fn group_artifacts(wh: &Warehouse, events: &[Event]) -> std::io::Result<Vec<Group>> {
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
    for ev in events.iter().filter(|e| e.kind == Kind::Artifact) {
        let (Some(src), Some(a)) = (&ev.source, &ev.artifact) else {
            continue;
        };
        let Some(hash) = &a.hash else { continue };
        let key = (src.scope.clone(), hash.clone());
        let text = if let Some(c) = &a.content {
            Some(c.clone())
        } else if let Some(b) = &a.blob {
            crate::blob::get_blob(&wh.layout, b)?.map(|v| String::from_utf8_lossy(&v).into_owned())
        } else {
            None
        };
        match groups.get_mut(&key) {
            Some(g) => {
                g.first_seen = g.first_seen.min(ev.ts);
                g.last_seen = g.last_seen.max(ev.ts);
                g.seen += 1;
                if g.text.is_none() {
                    g.text = text;
                }
            }
            None => {
                groups.insert(
                    key,
                    Group {
                        scope: src.scope.clone(),
                        hash: hash.clone(),
                        kind: a.kind.clone(),
                        locator: a.locator.clone(),
                        bytes: a.bytes,
                        redacted: a.redacted,
                        first_seen: ev.ts,
                        last_seen: ev.ts,
                        seen: 1,
                        text,
                    },
                );
            }
        }
    }
    Ok(groups.into_values().collect())
}

/// Removes every Artifact/Chunk node in the warehouse's scopes (edges cascade).
fn remove_derived(db: &Db, wh: &Warehouse) -> Result<usize, WarehouseError> {
    let mut scopes: Vec<Scope> = wh.manifest.scopes.iter().map(|x| parse_scope(x)).collect();
    scopes.push(Scope::Shared);
    let set = topodb_json::scopes_to_scope_set(&scopes);
    let mut ops = Vec::new();
    for label in [CHUNK_LABEL, ARTIFACT_LABEL] {
        for n in db.nodes_by_label_unbumped(&set, label) {
            ops.push(Op::RemoveNode { id: n.id });
        }
    }
    let n = ops.len();
    for batch in ops.chunks(500) {
        db.submit(batch.to_vec())?;
    }
    Ok(n)
}

pub fn derive(
    db: &Db,
    wh: &Warehouse,
    embed: Option<EmbedFn>,
    now_ms: i64,
    rederive: bool,
) -> Result<DeriveReport, WarehouseError> {
    let mut rep = DeriveReport::default();
    if rederive {
        rep.removed = remove_derived(db, wh)?;
    }
    let events = wh.events()?;
    // resolve the embed model name once (probe), so the stamp is stable across chunks
    let model: Option<String> = embed.and_then(|f| f("probe").map(|(m, _)| m));
    rep.embed_model = model.clone();
    let stamp = derived_by_stamp(model.as_deref());
    let groups = group_artifacts(wh, &events)?;
    let mut ids: HashMap<(String, String), NodeId> = HashMap::new();

    for g in &groups {
        let scope = parse_scope(&g.scope);
        let set = scope_set_for(scope);
        let id = artifact_node_id(&g.scope, &g.hash, g.first_seen);
        ids.insert((g.scope.clone(), g.hash.clone()), id);
        let mut ops: Vec<Op> = Vec::new();
        match db.node(&set, id) {
            // Any existing Artifact node at this id — regardless of its
            // `derived_by` stamp — is treated as already-derived. Without
            // `--rederive` we never remove/recreate it: a stamp mismatch
            // (e.g. daemon derive with an embedder, then a plain CLI
            // `derive`) would otherwise emit RemoveNode+CreateNode for the
            // same id in one batch, which the engine rejects, and would
            // collide chunk ids too. Stamp migration is exclusively
            // `--rederive`'s job (its removal runs in separate batches, see
            // `remove_derived` above).
            Some(existing) => {
                // update sighting stats if they moved
                let last = existing.props.get("last_seen").cloned();
                if last != Some(PropValue::DateTime(g.last_seen)) {
                    let mut p = BTreeMap::new();
                    p.insert(
                        "last_seen".to_string(),
                        Some(PropValue::DateTime(g.last_seen)),
                    );
                    p.insert("seen_count".to_string(), Some(PropValue::Int(g.seen)));
                    ops.push(Op::SetNodeProps { id, props: p });
                    rep.artifacts_updated += 1;
                }
            }
            None => {
                let mut props = Props::new();
                props.insert("type".into(), s(kind_str(&g.kind)));
                props.insert("locator".into(), s(&g.locator));
                props.insert("hash".into(), s(&g.hash));
                props.insert("bytes".into(), PropValue::Int(g.bytes as i64));
                props.insert("redacted".into(), PropValue::Bool(g.redacted));
                props.insert("first_seen".into(), PropValue::DateTime(g.first_seen));
                props.insert("last_seen".into(), PropValue::DateTime(g.last_seen));
                props.insert("seen_count".into(), PropValue::Int(g.seen));
                props.insert(DERIVED_BY_PROP.into(), s(&stamp));
                props.insert(TIER_PROP.into(), s("hot"));
                ops.push(Op::CreateNode {
                    id,
                    scope,
                    label: ARTIFACT_LABEL.into(),
                    props,
                });
                rep.artifacts_created += 1;
                match &g.text {
                    None => rep.pointer_only += 1,
                    Some(text) => {
                        for c in chunk_artifact(&g.kind, text) {
                            let cid = chunk_node_id(id, c.idx, DERIVE_VERSION, g.first_seen);
                            let mut cp = Props::new();
                            cp.insert("text".into(), s(&c.text));
                            cp.insert(
                                "preview".into(),
                                s(&c.text.chars().take(PREVIEW_CHARS).collect::<String>()),
                            );
                            cp.insert("idx".into(), PropValue::Int(c.idx as i64));
                            cp.insert("offset".into(), PropValue::Int(c.offset as i64));
                            cp.insert("len".into(), PropValue::Int(c.len as i64));
                            cp.insert("artifact_hash".into(), s(&g.hash));
                            cp.insert(DERIVED_BY_PROP.into(), s(&stamp));
                            ops.push(Op::CreateNode {
                                id: cid,
                                scope,
                                label: CHUNK_LABEL.into(),
                                props: cp,
                            });
                            if let Some((m, v)) = embed.and_then(|f| f(&c.text)) {
                                ops.push(Op::SetEmbedding {
                                    id: cid,
                                    model: m,
                                    vector: v,
                                });
                            }
                            let mut ep = Props::new();
                            ep.insert("idx".into(), PropValue::Int(c.idx as i64));
                            ops.push(Op::CreateEdge {
                                id: has_chunk_edge_id(id, cid, g.first_seen),
                                scope,
                                ty: HAS_CHUNK_EDGE.into(),
                                from: id,
                                to: cid,
                                props: ep,
                                valid_from: None,
                                recorded_at: None,
                            });
                            rep.chunks_created += 1;
                        }
                    }
                }
            }
        }
        if !ops.is_empty() {
            db.submit_at(ops, now_ms)?;
        }
    }

    // lineage
    for pick in evidence_windows(&events, wh.config.evidence_k) {
        let mscope = parse_scope(&pick.scope);
        let set = scope_set_for(mscope);
        let mut ops = Vec::new();
        for mid in &pick.memory_ids {
            let Ok(mid) = mid.parse::<NodeId>() else {
                rep.markers_skipped += 1;
                continue;
            };
            let Some(mem) = db.node(&set, mid) else {
                rep.markers_skipped += 1;
                continue;
            };
            let n = pick.artifacts.len();
            for (rank_from_oldest, (ascope, hash)) in pick.artifacts.iter().enumerate() {
                let Some(&aid) = ids.get(&(ascope.clone(), hash.clone())) else {
                    continue;
                };
                let art_scope = parse_scope(ascope);
                let edge_scope = match (mem.scope, art_scope) {
                    (Scope::Id(a), Scope::Id(b)) if a != b => {
                        rep.cross_scope_skipped += 1;
                        continue; // cross-project: not representable
                    }
                    (Scope::Id(a), _) => Scope::Id(a),
                    (Scope::Shared, other) => other,
                };
                let existing: Vec<EdgeRecord> = db.edges_from(
                    &scope_set_for(edge_scope),
                    mid,
                    Some(aid),
                    Some(EVIDENCE_EDGE),
                    true,
                    TimeAxis::Valid,
                )?;
                if !existing.is_empty() {
                    continue;
                }
                let mut ep = Props::new();
                ep.insert("rule".into(), s(EVIDENCE_RULE));
                ep.insert("session".into(), s(&pick.session));
                ep.insert(
                    "rank".into(),
                    PropValue::Int((n - 1 - rank_from_oldest) as i64),
                );
                ops.push(Op::CreateEdge {
                    id: evidence_edge_id(mid, aid, EVIDENCE_RULE, pick.ts),
                    scope: edge_scope,
                    ty: EVIDENCE_EDGE.into(),
                    from: mid,
                    to: aid,
                    props: ep,
                    valid_from: Some(pick.ts),
                    recorded_at: None,
                });
                rep.evidence_edges += 1;
            }
        }
        if !ops.is_empty() {
            db.submit_at(ops, now_ms)?;
        }
    }
    crate::report::set_last_run(db, "derive", now_ms)?;
    Ok(rep)
}
