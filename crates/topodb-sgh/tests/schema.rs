use topodb_sgh::schema::{Graph, NodeKind};

#[test]
fn parses_a_two_node_graph() {
    let src = include_str!("fixtures/simple.yaml");
    let g = Graph::from_yaml(src).expect("parses");

    assert_eq!(g.version, 1);
    assert_eq!(g.goal, "port the search analyzer");
    assert_eq!(g.nodes.len(), 2);

    assert_eq!(g.nodes[0].id, "survey");
    assert_eq!(g.nodes[0].kind, NodeKind::Agent);
    assert_eq!(g.nodes[0].needs, Vec::<String>::new());
    assert_eq!(g.nodes[0].budget.retries, 2);
    assert_eq!(g.nodes[0].budget.repairs, 1);

    assert_eq!(g.nodes[1].id, "build");
    assert_eq!(g.nodes[1].kind, NodeKind::Command);
    assert_eq!(g.nodes[1].needs, vec!["survey".to_string()]);
    assert_eq!(g.nodes[1].run.as_deref(), Some("cargo build -p topodb"));
}

#[test]
fn rejects_unknown_node_kind() {
    let src = "version: 1\ngoal: g\nnodes:\n  - id: a\n    kind: wizard\n    budget: {retries: 0, repairs: 0}\n";
    assert!(Graph::from_yaml(src).is_err());
}
