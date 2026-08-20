//! Graph snapshot: the one struct every `topodb graph` output format renders.
//! Deterministic by construction — no wall-clock fields, sorted collections.

use serde::{Deserialize, Serialize};
use topodb::{EdgeRecord, NodeRecord, PropValue, SmolStr};

use crate::{
    scope_label, ENTITY_LABEL, ENTITY_NAME_PROP, MEMORY_CONTENT_PROP, MEMORY_TOMBSTONE_PROPS,
};

pub const GRAPH_SNAPSHOT_VERSION: u32 = 1;
pub const GRAPH_DEFAULT_LIMIT: usize = 500;
pub const GRAPH_TITLE_MAX_CHARS: usize = 120;
pub const GRAPH_MERMAID_INLINE_MAX_NODES: usize = 60;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GraphSnapshot {
    pub snapshot_version: u32,
    pub db_path: Option<String>,
    pub op_seq: u64,
    pub scopes: Vec<String>,
    pub view: GraphView,
    pub truncated: Option<GraphTruncation>,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GraphView {
    pub kind: String,
    pub seeds: Vec<String>,
    pub query: Option<String>,
    pub hops: u8,
    pub as_of: Option<i64>,
    pub time_axis: String,
    pub direction: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GraphTruncation {
    pub nodes_dropped: usize,
    pub edges_dropped: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub title: String,
    pub scope: String,
    pub superseded: bool,
    pub hop: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub ty: String,
    pub scope: String,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}

/// Entity name, else `content` preview (≤ GRAPH_TITLE_MAX_CHARS chars,
/// char-boundary safe, '…' suffix when cut), else the label itself.
pub fn node_title(n: &NodeRecord) -> String {
    let titled = if n.label == ENTITY_LABEL {
        n.props.get(ENTITY_NAME_PROP)
    } else {
        n.props.get(MEMORY_CONTENT_PROP)
    };
    let s = match titled {
        Some(PropValue::Str(s)) => s.as_str(),
        _ => return n.label.to_string(),
    };
    // Whitespace-normalize so multi-line content stays a one-line title.
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= GRAPH_TITLE_MAX_CHARS {
        flat
    } else {
        let mut t: String = flat.chars().take(GRAPH_TITLE_MAX_CHARS).collect();
        t.push('…');
        t
    }
}

pub fn node_superseded(n: &NodeRecord) -> bool {
    MEMORY_TOMBSTONE_PROPS
        .iter()
        .any(|p| n.props.contains_key(*p))
}

pub fn graph_node(n: &NodeRecord, hop: u32) -> GraphNode {
    GraphNode {
        id: n.id.to_string(),
        label: n.label.to_string(),
        title: node_title(n),
        scope: scope_label(&n.scope),
        superseded: node_superseded(n),
        hop,
    }
}

pub fn graph_edge(e: &EdgeRecord) -> GraphEdge {
    GraphEdge {
        from: e.from.to_string(),
        to: e.to.to_string(),
        ty: e.ty.to_string(),
        scope: scope_label(&e.scope),
        valid_from: e.valid_from,
        valid_to: e.valid_to,
    }
}

pub fn to_canonical_json(s: &GraphSnapshot) -> Result<String, String> {
    serde_json::to_string(s).map_err(|e| format!("serializing snapshot: {e}"))
}

/// Parameters for ego snapshot building.
#[derive(Clone, Debug)]
pub struct EgoParams {
    pub seeds: Vec<topodb::NodeId>,
    pub query: Option<String>,
    pub query_k: usize,
    pub max_hops: u8,
    pub direction: topodb::Direction,
    pub edge_types: Option<Vec<SmolStr>>,
    pub as_of: Option<i64>,
    pub time_axis: topodb::TimeAxis,
}

/// Compute hop distances from seeds via undirected BFS over edges.
/// Returns a map of node id → hop distance.
fn hops_from(
    seeds: &[String],
    edges: &[GraphEdge],
    node_ids: &[String],
) -> std::collections::BTreeMap<String, u32> {
    use std::collections::{BTreeMap, HashSet, VecDeque};

    let mut hops: BTreeMap<String, u32> = BTreeMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();

    // Initialize seeds with hop 0
    for seed in seeds {
        hops.insert(seed.clone(), 0);
        visited.insert(seed.clone());
        queue.push_back((seed.clone(), 0));
    }

    // Build undirected adjacency map
    let mut adjacency: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in edges {
        adjacency
            .entry(edge.from.clone())
            .or_insert_with(Vec::new)
            .push(edge.to.clone());
        adjacency
            .entry(edge.to.clone())
            .or_insert_with(Vec::new)
            .push(edge.from.clone());
    }

    // BFS
    while let Some((node_id, hop)) = queue.pop_front() {
        if let Some(neighbors) = adjacency.get(&node_id) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    visited.insert(neighbor.clone());
                    let next_hop = hop + 1;
                    hops.insert(neighbor.clone(), next_hop);
                    queue.push_back((neighbor.clone(), next_hop));
                }
            }
        }
    }

    // Set unreachable nodes (defensive) to max_hops
    for node_id in node_ids {
        hops.entry(node_id.clone()).or_insert(u32::MAX);
    }

    hops
}

/// Build an ego-view snapshot from seeds and optional query.
pub fn build_ego(
    db: &topodb::Db,
    scopes: &topodb::ScopeSet,
    p: &EgoParams,
) -> Result<GraphSnapshot, String> {
    use std::collections::BTreeSet;

    // 1. Combine seeds and query results
    let mut all_seeds: BTreeSet<topodb::NodeId> = p.seeds.iter().cloned().collect();

    if let Some(query) = &p.query {
        let hits = db
            .search_text(scopes, query, p.query_k)
            .map_err(|e| format!("search_text: {e}"))?;
        for (hit, _score) in hits {
            all_seeds.insert(hit.id);
        }
    }

    if all_seeds.is_empty() {
        return Err("no seeds: pass --seed or a --query with hits".to_string());
    }

    // Convert to Vec and sort for determinism (by string representation)
    let seeds_vec: Vec<topodb::NodeId> = all_seeds.into_iter().collect();
    let seeds_str_vec: Vec<String> = seeds_vec.iter().map(|s| s.to_string()).collect();

    // 2. Build TraversalQuery and traverse
    let query = topodb::TraversalQuery {
        scopes: scopes.clone(),
        seeds: seeds_vec.clone(),
        max_hops: p.max_hops,
        edge_types: p.edge_types.clone(),
        direction: p.direction,
        as_of: p.as_of,
        time_axis: p.time_axis,
    };

    let subgraph = db.traverse(&query).map_err(|e| format!("traverse: {e}"))?;

    // 3. Convert nodes and compute hops
    let mut nodes: Vec<GraphNode> = subgraph
        .nodes
        .iter()
        .map(|n| graph_node(n, 0)) // placeholder hop, will update
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id)); // Sort by id

    // Compute hops
    let edges_for_bfs: Vec<GraphEdge> = subgraph.edges.iter().map(|e| graph_edge(e)).collect();

    let node_ids: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let hops_map = hops_from(&seeds_str_vec, &edges_for_bfs, &node_ids);

    for node in &mut nodes {
        node.hop = *hops_map.get(&node.id).unwrap_or(&u32::MAX);
    }

    // 4. Sort edges
    let mut edges_raw = subgraph.edges.clone();
    edges_raw.sort_by(|a, b| a.id.cmp(&b.id)); // Sort EdgeRecords by id first
    let mut edges: Vec<GraphEdge> = edges_raw.iter().map(|e| graph_edge(e)).collect();
    // Stable sort by (from, to, ty)
    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then_with(|| a.to.cmp(&b.to))
            .then_with(|| a.ty.cmp(&b.ty))
    });

    // 5. Get op_seq and scopes
    let op_seq = db.current_seq().map_err(|e| format!("current_seq: {e}"))?;

    let scope_labels: Vec<String> = scopes.iter_scopes().map(|s| scope_label(&s)).collect();

    // 6. Direction and TimeAxis to lowercase strings
    let direction_str = match p.direction {
        topodb::Direction::Out => "out",
        topodb::Direction::In => "in",
        topodb::Direction::Both => "both",
    };

    let time_axis_str = match p.time_axis {
        topodb::TimeAxis::Valid => "valid",
        topodb::TimeAxis::Recorded => "recorded",
    };

    Ok(GraphSnapshot {
        snapshot_version: GRAPH_SNAPSHOT_VERSION,
        db_path: None,
        op_seq,
        scopes: scope_labels,
        view: GraphView {
            kind: "ego".to_string(),
            seeds: seeds_str_vec,
            query: p.query.clone(),
            hops: p.max_hops,
            as_of: p.as_of,
            time_axis: time_axis_str.to_string(),
            direction: direction_str.to_string(),
        },
        truncated: None,
        nodes,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use topodb::{EdgeId, NodeId, Op, PropValue, Scope};

    fn node(label: &str, props: Vec<(&str, PropValue)>) -> topodb::NodeRecord {
        topodb::NodeRecord {
            id: NodeId::new(),
            scope: Scope::Shared,
            label: label.into(),
            props: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            embedding: None,
        }
    }

    /// a(Memory) -ABOUT-> b(Entity) -ABOUT-> c(Entity); returns (db, [a,b,c])
    fn seed_chain(dir: &tempfile::TempDir) -> (topodb::Db, [NodeId; 3]) {
        let db = topodb::Db::open_with(dir.path().join("t.redb"), crate::default_spec()).unwrap();
        let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
        db.submit(vec![
            Op::CreateNode {
                id: a,
                scope: Scope::Shared,
                label: "Memory".into(),
                props: [("content".to_string(), PropValue::Str("alpha fact".into()))]
                    .into_iter()
                    .collect(),
            },
            Op::CreateNode {
                id: b,
                scope: Scope::Shared,
                label: "Entity".into(),
                props: [("name".to_string(), PropValue::Str("Beta".into()))]
                    .into_iter()
                    .collect(),
            },
            Op::CreateNode {
                id: c,
                scope: Scope::Shared,
                label: "Entity".into(),
                props: [("name".to_string(), PropValue::Str("Gamma".into()))]
                    .into_iter()
                    .collect(),
            },
            Op::CreateEdge {
                id: EdgeId::new(),
                scope: Scope::Shared,
                ty: "ABOUT".into(),
                from: a,
                to: b,
                props: Default::default(),
                valid_from: None,
                recorded_at: None,
            },
            Op::CreateEdge {
                id: EdgeId::new(),
                scope: Scope::Shared,
                ty: "ABOUT".into(),
                from: b,
                to: c,
                props: Default::default(),
                valid_from: None,
                recorded_at: None,
            },
        ])
        .unwrap();
        (db, [a, b, c])
    }

    #[test]
    fn ego_walks_hops_and_labels_them() {
        let dir = tempfile::tempdir().unwrap();
        let (db, [a, _b, c]) = seed_chain(&dir);
        let scopes = crate::scope_to_scope_set(Scope::Shared);
        let p = EgoParams {
            seeds: vec![a],
            query: None,
            query_k: 3,
            max_hops: 2,
            direction: topodb::Direction::Both,
            edge_types: None,
            as_of: None,
            time_axis: topodb::TimeAxis::Valid,
        };
        let snap = build_ego(&db, &scopes, &p).unwrap();
        assert_eq!(snap.nodes.len(), 3);
        assert_eq!(snap.edges.len(), 2);
        assert_eq!(snap.view.kind, "ego");
        let hop_of = |id: NodeId| {
            snap.nodes
                .iter()
                .find(|n| n.id == id.to_string())
                .unwrap()
                .hop
        };
        assert_eq!(hop_of(a), 0);
        assert_eq!(hop_of(c), 2);
        // determinism: sorted node ids
        let ids: Vec<_> = snap.nodes.iter().map(|n| n.id.clone()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn ego_query_seeds_from_search_hits() {
        let dir = tempfile::tempdir().unwrap();
        let (db, [a, ..]) = seed_chain(&dir);
        let scopes = crate::scope_to_scope_set(Scope::Shared);
        let p = EgoParams {
            seeds: vec![],
            query: Some("alpha".into()),
            query_k: 3,
            max_hops: 1,
            direction: topodb::Direction::Both,
            edge_types: None,
            as_of: None,
            time_axis: topodb::TimeAxis::Valid,
        };
        let snap = build_ego(&db, &scopes, &p).unwrap();
        assert!(snap.nodes.iter().any(|n| n.id == a.to_string()));
        assert_eq!(snap.view.query.as_deref(), Some("alpha"));
    }

    #[test]
    fn ego_no_seeds_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = seed_chain(&dir);
        let scopes = crate::scope_to_scope_set(Scope::Shared);
        let p = EgoParams {
            seeds: vec![],
            query: Some("zzzznohit".into()),
            query_k: 3,
            max_hops: 1,
            direction: topodb::Direction::Both,
            edge_types: None,
            as_of: None,
            time_axis: topodb::TimeAxis::Valid,
        };
        assert!(build_ego(&db, &scopes, &p).is_err());
    }

    #[test]
    fn title_prefers_name_for_entities_and_previews_memory_content() {
        let e = node("Entity", vec![("name", PropValue::Str("Alice".into()))]);
        assert_eq!(node_title(&e), "Alice");
        let long = "x".repeat(300);
        let m = node("Memory", vec![("content", PropValue::Str(long))]);
        let t = node_title(&m);
        assert!(t.chars().count() <= GRAPH_TITLE_MAX_CHARS + 1); // +1 for the ellipsis
        assert!(t.ends_with('…'));
    }

    #[test]
    fn title_truncates_on_char_boundary_not_bytes() {
        let m = node("Memory", vec![("content", PropValue::Str("é".repeat(200)))]);
        let t = node_title(&m); // must not panic on a multi-byte boundary
        assert!(t.ends_with('…'));
    }

    #[test]
    fn title_falls_back_to_label_when_no_titled_prop() {
        let n = node("Widget", vec![("count", PropValue::Int(3))]);
        assert_eq!(node_title(&n), "Widget");
    }

    #[test]
    fn superseded_detects_tombstone_props() {
        let live = node("Memory", vec![("content", PropValue::Str("a".into()))]);
        assert!(!node_superseded(&live));
        let dead = node(
            "Memory",
            vec![
                ("content", PropValue::Str("a".into())),
                ("superseded_at", PropValue::DateTime(42)),
            ],
        );
        assert!(node_superseded(&dead));
        let forgotten = node(
            "Memory",
            vec![
                ("content", PropValue::Str("a".into())),
                ("forgotten_at", PropValue::DateTime(42)),
            ],
        );
        assert!(node_superseded(&forgotten));
    }

    #[test]
    fn canonical_json_is_stable_and_round_trips() {
        let snap = GraphSnapshot {
            snapshot_version: GRAPH_SNAPSHOT_VERSION,
            db_path: None,
            op_seq: 7,
            scopes: vec!["shared".into()],
            view: GraphView {
                kind: "ego".into(),
                seeds: vec!["01X".into()],
                query: None,
                hops: 2,
                as_of: None,
                time_axis: "valid".into(),
                direction: "both".into(),
            },
            truncated: None,
            nodes: vec![],
            edges: vec![],
        };
        let a = to_canonical_json(&snap).unwrap();
        let b = to_canonical_json(&snap).unwrap();
        assert_eq!(a, b);
        let back: GraphSnapshot = serde_json::from_str(&a).unwrap();
        assert_eq!(back, snap);
    }
}
