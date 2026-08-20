//! Graph snapshot: the one struct every `topodb graph` output format renders.
//! Deterministic by construction — no wall-clock fields, sorted collections.

use serde::{Deserialize, Serialize};
use topodb::{EdgeRecord, NodeRecord, PropValue};

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

#[cfg(test)]
mod tests {
    use super::*;
    use topodb::{NodeId, PropValue, Scope};

    fn node(label: &str, props: Vec<(&str, PropValue)>) -> topodb::NodeRecord {
        topodb::NodeRecord {
            id: NodeId::new(),
            scope: Scope::Shared,
            label: label.into(),
            props: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            embedding: None,
        }
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
