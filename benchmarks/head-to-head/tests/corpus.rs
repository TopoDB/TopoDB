use head_to_head::corpus::Corpus;

#[test]
fn generation_is_deterministic_for_a_seed() {
    let a = Corpus::generate(42, 100);
    let b = Corpus::generate(42, 100);
    assert_eq!(a.nodes, b.nodes, "same seed must give the same nodes");
    assert_eq!(a.edges, b.edges, "same seed must give the same edges");
}

#[test]
fn different_seeds_give_different_corpora() {
    let a = Corpus::generate(1, 100);
    let b = Corpus::generate(2, 100);
    assert_ne!(a.nodes, b.nodes);
}

#[test]
fn translation_ratio_is_reported_and_correct() {
    let c = Corpus::generate(7, 50);
    let r = c.translation_ratio();

    assert_eq!(r.nodes, 50);
    // Each node contributes one fact per prop; each edge contributes one fact.
    let expected_facts = r.props + r.edges;
    assert_eq!(
        r.facts, expected_facts,
        "the EAV fact count must equal props + edges — this ratio goes in every report"
    );
    assert!(r.props > r.nodes, "nodes carry multiple props, else the shapes are trivially equal");
}

#[test]
fn every_edge_references_nodes_that_exist() {
    let c = Corpus::generate(3, 200);
    for e in &c.edges {
        assert!(e.from < c.nodes.len(), "edge source in range");
        assert!(e.to < c.nodes.len(), "edge target in range");
    }
}

#[test]
fn the_graph_is_connected_enough_to_traverse_four_hops() {
    // A k-hop benchmark is meaningless on a disconnected graph.
    let c = Corpus::generate(5, 500);
    let reachable = c.reachable_within(0, 4);
    assert!(
        reachable > 10,
        "seed node must reach >10 nodes in 4 hops, got {reachable}"
    );
}

#[test]
fn no_duplicate_edges_are_generated() {
    // TopoDB stores each CreateEdge as a distinct object; minigraf's EAV
    // identity collapses a repeated (from, to, ty) into one fact. A duplicate
    // therefore makes the two engines store different amounts of data and
    // silently invalidates the published translation ratio.
    for seed in [1u64, 11, 42, 20260718] {
        let c = Corpus::generate(seed, 200);
        let mut seen = std::collections::HashSet::new();
        for e in &c.edges {
            assert!(
                seen.insert((e.from, e.to, e.ty.clone())),
                "duplicate edge ({}, {}, {}) at seed {seed}",
                e.from, e.to, e.ty
            );
        }
    }
}
