use head_to_head::corpus::Corpus;
use head_to_head::engine::{AsOfSupport, Engine};
use head_to_head::topodb_driver::TopoDbDriver;

fn loaded_topodb(n: usize) -> (tempfile::TempDir, TopoDbDriver, Corpus) {
    let dir = tempfile::tempdir().unwrap();
    let corpus = Corpus::generate(11, n);
    let mut db = TopoDbDriver::open(&dir.path().join("bench.redb")).unwrap();
    db.insert_corpus(&corpus).unwrap();
    (dir, db, corpus)
}

#[test]
fn topodb_point_lookup_returns_the_stored_payload() {
    let (_d, db, corpus) = loaded_topodb(50);
    let payload = db.point_lookup(7).unwrap().expect("node 7 exists");
    assert_eq!(payload.name, corpus.nodes[7].name);
    assert_eq!(payload.rank, corpus.nodes[7].rank);
}

#[test]
fn topodb_point_lookup_misses_are_none_not_errors() {
    let (_d, db, _c) = loaded_topodb(50);
    assert!(db.point_lookup(9_999).unwrap().is_none());
}

#[test]
fn topodb_k_hop_reaches_more_nodes_at_greater_depth() {
    let (_d, db, _c) = loaded_topodb(300);
    let d1 = db.k_hop(0, 1).unwrap();
    let d3 = db.k_hop(0, 3).unwrap();
    assert!(d3 > d1, "depth 3 ({d3}) must reach more than depth 1 ({d1})");
}

#[test]
fn topodb_reports_as_of_as_unsupported() {
    // TopoDB cannot return a historical node payload: node props are
    // non-temporal (op.rs:19-20). Saying so is the honest result, and it is
    // what Phase 2a exists to fix.
    assert_eq!(TopoDbDriver::as_of_support(), AsOfSupport::NodePayloadUnsupported);
}

#[test]
fn topodb_reports_nonzero_on_disk_size() {
    let (_d, db, _c) = loaded_topodb(100);
    assert!(db.on_disk_bytes().unwrap() > 0);
}

#[test]
fn topodb_k_hop_matches_corpus_reachable_within_seed_counting() {
    // Task 2's review flagged seed-node counting parity as the highest-risk
    // detail: `Corpus::reachable_within` seeds `seen` with the seed index
    // itself before walking, so its count includes the seed node. This test
    // asserts `k_hop` counts the same way, against the same corpus, for the
    // same seed/depth pairs, empirically rather than by assumption.
    let (_d, db, corpus) = loaded_topodb(300);
    for depth in 1..=4u8 {
        let expected = corpus.reachable_within(0, depth);
        let actual = db.k_hop(0, depth).unwrap();
        assert_eq!(
            actual, expected,
            "k_hop({depth}) = {actual} but reachable_within(0, {depth}) = {expected}"
        );
    }
}
