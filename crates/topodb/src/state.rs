use crate::ids::{EdgeId, NodeId, Scope};
use crate::props::Props;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub scope: Scope,
    pub label: SmolStr,
    pub props: Props,
    pub embedding: Option<(String, Vec<f32>)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub id: EdgeId,
    pub scope: Scope,
    pub ty: SmolStr,
    pub from: NodeId,
    pub to: NodeId,
    pub props: Props,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::op::Op;
    use crate::storage::Storage;

    fn db() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::create(dir.path().join("t.redb")).unwrap();
        (dir, s)
    }

    #[test]
    fn create_edge_resolves_valid_from_and_persists() {
        let (_d, s) = db();
        let scope = Scope::Id(ScopeId::new());
        let (a, b, e) = (NodeId::new(), NodeId::new(), EdgeId::new());
        let batch = s
            .apply_batch(
                vec![
                    Op::CreateNode { id: a, scope, label: "Memory".into(), props: Default::default() },
                    Op::CreateNode { id: b, scope, label: "Entity".into(), props: Default::default() },
                    Op::CreateEdge {
                        id: e,
                        scope,
                        ty: "ABOUT".into(),
                        from: a,
                        to: b,
                        props: Default::default(),
                        valid_from: None,
                    },
                ],
                1_000,
            )
            .unwrap();
        // Stored op is fully resolved:
        match &batch.resolved[2] {
            Op::CreateEdge { valid_from, .. } => assert_eq!(*valid_from, Some(1_000)),
            other => panic!("unexpected {other:?}"),
        }
        let edge = s.load_edge(e).unwrap().unwrap();
        assert_eq!(edge.valid_from, 1_000);
        assert_eq!(edge.valid_to, None);
    }

    #[test]
    fn cross_scope_edge_requires_shared_endpoint() {
        let (_d, s) = db();
        let (s1, s2) = (Scope::Id(ScopeId::new()), Scope::Id(ScopeId::new()));
        let (a, b) = (NodeId::new(), NodeId::new());
        let err = s
            .apply_batch(
                vec![
                    Op::CreateNode { id: a, scope: s1, label: "Memory".into(), props: Default::default() },
                    Op::CreateNode { id: b, scope: s2, label: "Entity".into(), props: Default::default() },
                    Op::CreateEdge {
                        id: EdgeId::new(),
                        scope: s1,
                        ty: "ABOUT".into(),
                        from: a,
                        to: b,
                        props: Default::default(),
                        valid_from: None,
                    },
                ],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, crate::TopoError::Rejected(_)));
        // Whole batch rejected — nodes must NOT exist:
        assert!(s.load_node(a).unwrap().is_none());

        // With a Shared endpoint it works:
        s.apply_batch(
            vec![
                Op::CreateNode { id: a, scope: s1, label: "Memory".into(), props: Default::default() },
                Op::CreateNode { id: b, scope: Scope::Shared, label: "Entity".into(), props: Default::default() },
                Op::CreateEdge {
                    id: EdgeId::new(),
                    scope: s1,
                    ty: "ABOUT".into(),
                    from: a,
                    to: b,
                    props: Default::default(),
                    valid_from: None,
                },
            ],
            0,
        )
        .unwrap();
    }

    #[test]
    fn remove_node_removes_incident_edges() {
        let (_d, s) = db();
        let scope = Scope::Id(ScopeId::new());
        let (a, b, e) = (NodeId::new(), NodeId::new(), EdgeId::new());
        s.apply_batch(
            vec![
                Op::CreateNode { id: a, scope, label: "M".into(), props: Default::default() },
                Op::CreateNode { id: b, scope, label: "M".into(), props: Default::default() },
                Op::CreateEdge {
                    id: e,
                    scope,
                    ty: "RELATES_TO".into(),
                    from: a,
                    to: b,
                    props: Default::default(),
                    valid_from: None,
                },
            ],
            0,
        )
        .unwrap();
        s.apply_batch(vec![Op::RemoveNode { id: a }], 1).unwrap();
        assert!(s.load_node(a).unwrap().is_none());
        assert!(s.load_edge(e).unwrap().is_none());
    }
}
