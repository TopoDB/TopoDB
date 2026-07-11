//! Naive reference model for the disk-resident-read migration.
//!
//! This intentionally uses no engine internals: it is the stable oracle for
//! public read semantics while v3 replaces the snapshot implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use topodb::workload::{batches, WorkloadSpec};
use topodb::*;

mod reference {
    use super::*;

    #[derive(Default)]
    pub struct RefModel {
        pub nodes: HashMap<NodeId, NodeRecord>,
        pub edges: HashMap<EdgeId, EdgeRecord>,
    }

    impl RefModel {
        pub fn apply(&mut self, op: &Op) {
            match op {
                Op::CreateNode {
                    id,
                    scope,
                    label,
                    props,
                } => {
                    self.nodes.insert(
                        *id,
                        NodeRecord {
                            id: *id,
                            scope: *scope,
                            label: label.clone(),
                            props: props.clone(),
                            embedding: None,
                        },
                    );
                }
                Op::SetNodeProps { id, props } => {
                    if let Some(node) = self.nodes.get_mut(id) {
                        for (key, value) in props {
                            match value {
                                Some(value) => {
                                    node.props.insert(key.clone(), value.clone());
                                }
                                None => {
                                    node.props.remove(key);
                                }
                            }
                        }
                    }
                }
                Op::SetEmbedding { id, model, vector } => {
                    if let Some(node) = self.nodes.get_mut(id) {
                        node.embedding = Some((model.clone(), vector.clone()));
                    }
                }
                Op::RemoveNode { id } => {
                    self.nodes.remove(id);
                    self.edges
                        .retain(|_, edge| edge.from != *id && edge.to != *id);
                }
                Op::CreateEdge {
                    id,
                    scope,
                    ty,
                    from,
                    to,
                    props,
                    valid_from,
                } => {
                    self.edges.insert(
                        *id,
                        EdgeRecord {
                            id: *id,
                            scope: *scope,
                            ty: ty.clone(),
                            from: *from,
                            to: *to,
                            props: props.clone(),
                            valid_from: valid_from.expect("test ops are resolved"),
                            valid_to: None,
                        },
                    );
                }
                Op::CloseEdge { id, valid_to } => {
                    if let Some(edge) = self.edges.get_mut(id) {
                        edge.valid_to = *valid_to;
                    }
                }
            }
        }

        pub fn get(&self, id: NodeId, scopes: &ScopeSet) -> Option<NodeRecord> {
            self.nodes
                .get(&id)
                .filter(|node| scopes.contains(node.scope))
                .cloned()
        }

        pub fn find_by_prop(
            &self,
            scopes: &ScopeSet,
            label: &str,
            prop: &str,
            value: &PropValue,
        ) -> Vec<NodeRecord> {
            let mut hits: Vec<_> = self
                .nodes
                .values()
                .filter(|node| {
                    node.label == label
                        && scopes.contains(node.scope)
                        && node.props.get(prop) == Some(value)
                })
                .cloned()
                .collect();
            hits.sort_by_key(|node| node.id);
            hits
        }

        pub fn traverse(
            &self,
            start: NodeId,
            scopes: &ScopeSet,
            max_hops: u8,
            as_of: i64,
            direction: Direction,
        ) -> (BTreeSet<NodeId>, BTreeSet<EdgeId>) {
            let mut nodes = BTreeSet::new();
            let mut edges = BTreeSet::new();
            let mut frontier = VecDeque::new();
            if self
                .nodes
                .get(&start)
                .is_some_and(|node| scopes.contains(node.scope))
            {
                nodes.insert(start);
                frontier.push_back((start, 0));
            }
            while let Some((current, hop)) = frontier.pop_front() {
                if hop >= max_hops {
                    continue;
                }
                for edge in self.edges.values() {
                    let other = match direction {
                        Direction::Out if edge.from == current => Some(edge.to),
                        Direction::In if edge.to == current => Some(edge.from),
                        Direction::Both if edge.from == current => Some(edge.to),
                        Direction::Both if edge.to == current => Some(edge.from),
                        _ => None,
                    };
                    let Some(other) = other else {
                        continue;
                    };
                    if !scopes.contains(edge.scope)
                        || edge.valid_from > as_of
                        || edge.valid_to.is_some_and(|valid_to| as_of >= valid_to)
                    {
                        continue;
                    }
                    if !self
                        .nodes
                        .get(&other)
                        .is_some_and(|node| scopes.contains(node.scope))
                    {
                        continue;
                    }
                    edges.insert(edge.id);
                    if nodes.insert(other) {
                        frontier.push_back((other, hop + 1));
                    }
                }
            }
            (nodes, edges)
        }
    }
}

#[derive(Clone)]
enum Probe {
    Get {
        id: NodeId,
        scopes: ScopeSet,
    },
    Find {
        scopes: ScopeSet,
        label: &'static str,
        prop: &'static str,
        value: PropValue,
    },
    Traverse {
        id: NodeId,
        scopes: ScopeSet,
        hops: u8,
        as_of: i64,
        direction: Direction,
    },
}

fn index_spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    }
}

fn apply(db: &Db, model: &mut reference::RefModel, ops: Vec<Op>) {
    for op in &ops {
        model.apply(op);
    }
    db.submit(ops).unwrap();
}

fn assert_equivalent(db: &Db, model: &reference::RefModel, probes: &[Probe]) {
    for probe in probes {
        match probe {
            Probe::Get { id, scopes } => {
                assert_eq!(db.node(scopes, *id), model.get(*id, scopes), "get {id:?}")
            }
            Probe::Find {
                scopes,
                label,
                prop,
                value,
            } => {
                let mut actual = db.nodes_by_prop(scopes, label, prop, value).unwrap();
                actual.sort_by_key(|node| node.id);
                assert_eq!(
                    actual,
                    model.find_by_prop(scopes, label, prop, value),
                    "find {label}.{prop}"
                );
            }
            Probe::Traverse {
                id,
                scopes,
                hops,
                as_of,
                direction,
            } => {
                let actual = db
                    .traverse(&TraversalQuery {
                        scopes: scopes.clone(),
                        seeds: vec![*id],
                        max_hops: *hops,
                        edge_types: None,
                        direction: *direction,
                        as_of: Some(*as_of),
                    })
                    .unwrap();
                let actual_nodes = actual
                    .nodes
                    .into_iter()
                    .map(|node| node.id)
                    .collect::<BTreeSet<_>>();
                let actual_edges = actual
                    .edges
                    .into_iter()
                    .map(|edge| edge.id)
                    .collect::<BTreeSet<_>>();
                let (expected_nodes, expected_edges) =
                    model.traverse(*id, scopes, *hops, *as_of, *direction);
                assert_eq!(actual_nodes, expected_nodes, "traverse nodes from {id:?}");
                assert_eq!(actual_edges, expected_edges, "traverse edges from {id:?}");
            }
        }
    }
}

#[test]
fn reference_model_matches_v2_engine_on_generated_workloads() {
    let spec = WorkloadSpec {
        memories: 500,
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("d.redb"), index_spec()).unwrap();
    let mut model = reference::RefModel::default();
    let mut edge_ids = Vec::new();
    for batch in batches(&spec) {
        edge_ids.extend(batch.iter().filter_map(|op| match op {
            Op::CreateEdge { id, .. } => Some(*id),
            _ => None,
        }));
        apply(&db, &mut model, batch);
    }
    let closed_at = 1_700_000_100_000;
    for id in edge_ids.into_iter().step_by(7) {
        apply(
            &db,
            &mut model,
            vec![Op::CloseEdge {
                id,
                valid_to: Some(closed_at),
            }],
        );
    }
    for i in (0..spec.memories).step_by(13) {
        apply(&db, &mut model, vec![Op::RemoveNode { id: memory_id(i) }]);
    }
    for i in (0..spec.memories).filter(|i| i % 5 == 0 && i % 13 != 0) {
        let mut props = BTreeMap::new();
        props.insert("reviewed".to_string(), Some(PropValue::Bool(true)));
        apply(
            &db,
            &mut model,
            vec![Op::SetNodeProps {
                id: memory_id(i),
                props,
            }],
        );
    }

    let own = ScopeId::from_u128(1);
    let scopes = [
        ScopeSet::of(&[own]),
        ScopeSet::of(&[ScopeId::from_u128(2)]),
        ScopeSet::of(&[own]).with_shared(),
    ];
    let mut probes = Vec::new();
    for i in 0..50 {
        probes.push(Probe::Get {
            id: memory_id(i),
            scopes: scopes[i % scopes.len()].clone(),
        });
    }
    for i in 0..20 {
        probes.push(Probe::Get {
            id: memory_id(i * 13),
            scopes: ScopeSet::of(&[own]),
        });
    }
    for i in [0, 7, 199, 499, 500] {
        probes.push(Probe::Find {
            scopes: ScopeSet::of(&[own]),
            label: "Entity",
            prop: "name",
            value: PropValue::Str(format!("entity-{i}")),
        });
    }
    let times = [1_699_999_999_999, 1_700_000_000_250, 1_700_000_200_000];
    for i in 1..=20 {
        if i % 13 == 0 {
            continue;
        }
        for hops in 1..=4 {
            for (scope_index, scope) in scopes.iter().enumerate() {
                probes.push(Probe::Traverse {
                    id: memory_id(i),
                    scopes: scope.clone(),
                    hops,
                    as_of: times[(i + hops as usize + scope_index) % times.len()],
                    direction: [Direction::Out, Direction::In, Direction::Both]
                        [(i + hops as usize) % 3],
                });
            }
        }
    }
    assert_equivalent(&db, &model, &probes);
}

fn memory_id(i: usize) -> NodeId {
    NodeId::from_u128(0x0100_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}
