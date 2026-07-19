use head_to_head::corpus::Corpus;
use head_to_head::engine::{AsOfSupport, Engine};
use head_to_head::minigraf_driver::MinigrafDriver;
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

fn loaded_minigraf(n: usize) -> (tempfile::TempDir, MinigrafDriver, Corpus) {
    let dir = tempfile::tempdir().unwrap();
    let corpus = Corpus::generate(11, n);
    let mut db = MinigrafDriver::open(&dir.path().join("bench.graph")).unwrap();
    db.insert_corpus(&corpus).unwrap();
    (dir, db, corpus)
}

#[test]
fn minigraf_point_lookup_returns_the_stored_payload() {
    let (_d, db, corpus) = loaded_minigraf(50);
    let payload = db.point_lookup(7).unwrap().expect("node 7 exists");
    assert_eq!(payload.name, corpus.nodes[7].name);
    assert_eq!(payload.rank, corpus.nodes[7].rank);
}

#[test]
fn minigraf_point_lookup_misses_are_none_not_errors() {
    let (_d, db, _c) = loaded_minigraf(50);
    assert!(db.point_lookup(9_999).unwrap().is_none());
}

/// `k_hop` is deliberately NOT a real traversal for this driver -- see the
/// module doc comment on `minigraf_driver.rs` for the full investigation
/// (recursive rules capped at 2 args; a materialised `or`-rule mixes
/// `Value::Ref`/`Value::Keyword` representations; even explicit `#uuid`
/// literals still undercounted real matches against this corpus's actual
/// data shape). Reporting an honest error, rather than a number that looks
/// comparable and isn't, is the correct outcome here: this asserts the
/// error is returned, not a plausible-looking wrong count.
#[test]
fn minigraf_k_hop_reports_not_comparable() {
    let (_d, db, _c) = loaded_minigraf(50);
    assert!(
        db.k_hop(0, 1).is_err(),
        "k_hop should report an honest error, not an unverified count"
    );
}

/// The fairness question Task 2's review left open: does `insert_corpus`
/// assert exactly what `Corpus::translation_ratio().facts` claims (one
/// triple per property, one per edge, nothing else), or does it need
/// bookkeeping facts beyond that which would make the published ratio
/// understate minigraf's real write volume? Verified against a real,
/// queried database, not by reading the source.
///
/// It does NOT need bookkeeping facts -- `insert_corpus` asserts exactly
/// `PROP_NAMES.len()` triples per node plus one per edge, nothing more.
///
/// This test previously also caught a real, separate defect: `Corpus::generate`
/// could produce two `LogicalEdge`s with the identical `(from, to, ty)`
/// triple (e.g. two `(3, 1, DERIVED_FROM)` edges for seed 11, size 10).
/// TopoDB stored those as two distinct edge objects, while minigraf's EAV
/// identity collapsed them into one fact, so `translation_ratio().facts`
/// silently overstated minigraf's real write volume. `Corpus::generate` now
/// deduplicates `(from, to, ty)` at generation time (see `corpus.rs`), so
/// `corpus.edges` is already distinct and `translation_ratio().facts` must
/// match minigraf's real stored fact count exactly, with no divergence left
/// to detect.
#[test]
fn minigraf_fact_count_matches_translation_ratio_exactly() {
    let (_d, db, corpus) = loaded_minigraf(10);
    let ratio = corpus.translation_ratio();

    let distinct_edges: std::collections::HashSet<(usize, usize, String)> = corpus
        .edges
        .iter()
        .map(|e| (e.from, e.to, e.ty.clone()))
        .collect();
    assert_eq!(
        distinct_edges.len(),
        corpus.edges.len(),
        "Corpus::generate must not produce duplicate (from, to, ty) edges"
    );

    let actual = db.fact_count().unwrap();
    assert_eq!(
        actual, ratio.facts,
        "insert_corpus should store exactly translation_ratio().facts \
         ({}), got {actual} -- the published fairness ratio must match reality",
        ratio.facts
    );
}

/// Both drivers must agree on what the corpus says. If they disagree, one of
/// them is storing or reading something the other is not, and every timing
/// comparison downstream is meaningless.
///
/// (There is no equivalent `both_engines_agree_on_k_hop_counts` test: k_hop
/// is not in the scored set for minigraf, see above.)
#[test]
fn both_engines_return_identical_payloads_for_the_same_corpus() {
    let (_d1, topo, corpus) = loaded_topodb(100);
    let (_d2, mini, _) = loaded_minigraf(100);

    for id in [0usize, 1, 17, 42, 99] {
        let a = topo.point_lookup(id).unwrap();
        let b = mini.point_lookup(id).unwrap();
        assert_eq!(a, b, "engines disagree on node {id} from corpus seed {}", corpus.seed);
    }
}
