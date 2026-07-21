//! Degenerate-input robustness for the query surface: empty scope sets, empty
//! seeds, zero hops, empty/oversized/unicode search queries, unknown ids and
//! labels. None of these should panic; each should return the obviously-correct
//! empty/rejected result. Boundary inputs are where a query engine tends to
//! panic or silently return wrong answers.

use topodb::*;

fn db_with_one_note(content: &str) -> (Db, tempfile::TempDir, ScopeId, NodeId) {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Note".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let s = ScopeId::new();
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    db.submit(vec![Op::CreateNode {
        id,
        scope: Scope::Id(s),
        label: "Note".into(),
        props,
    }])
    .unwrap();
    (db, dir, s, id)
}

#[test]
fn empty_scope_set_reads_see_nothing() {
    let (db, _d, _s, id) = db_with_one_note("hello world");
    let empty = ScopeSet::of(&[]);
    assert!(db.nodes_by_label(&empty, "Note").is_empty());
    assert!(db.node(&empty, id).is_none());
    // A search over no scopes returns no hits (not an error, not a panic).
    let hits = db.search_text(&empty, "hello", 10).unwrap();
    assert!(hits.is_empty(), "no scopes => no search hits, got {hits:?}");
}

#[test]
fn zero_k_search_is_rejected_not_panicking() {
    let (db, _d, s, _id) = db_with_one_note("hello world");
    let r = db.search_text(&ScopeSet::of(&[s]), "hello", 0);
    assert!(
        matches!(r, Err(TopoError::Rejected(_))),
        "k=0 => Rejected, got {r:?}"
    );
}

#[test]
fn empty_query_string_does_not_panic() {
    let (db, _d, s, _id) = db_with_one_note("hello world");
    // Empty/whitespace queries have no terms; must yield a clean result
    // (empty hits or an explicit rejection), never a panic.
    for q in ["", "   ", "\t\n"] {
        let r = db.search_text(&ScopeSet::of(&[s]), q, 5);
        assert!(
            matches!(&r, Ok(v) if v.is_empty()) || matches!(&r, Err(TopoError::Rejected(_))),
            "empty query {q:?} must be empty-or-rejected, got {r:?}"
        );
    }
}

#[test]
fn unicode_and_oversized_queries_do_not_panic() {
    let (db, _d, s, _id) = db_with_one_note("hello world");
    let scopes = ScopeSet::of(&[s]);
    // Multibyte scripts, emoji, and a very long query must all be handled.
    let _ = db.search_text(&scopes, "日本語のテキスト", 5).unwrap();
    let _ = db.search_text(&scopes, "🦀🦀🦀 rust", 5).unwrap();
    let big = "word ".repeat(5_000);
    let _ = db.search_text(&scopes, &big, 5).unwrap();
    // k far larger than the corpus just returns what exists.
    let hits = db.search_text(&scopes, "hello", 1_000_000).unwrap();
    assert!(hits.len() <= 1);
}

#[test]
fn unknown_label_and_id_return_empty() {
    let (db, _d, s, _id) = db_with_one_note("hello world");
    let scopes = ScopeSet::of(&[s]);
    assert!(db.nodes_by_label(&scopes, "NoSuchLabel").is_empty());
    assert!(db.nodes_by_label(&scopes, "").is_empty());
    assert!(
        db.node(&scopes, NodeId::new()).is_none(),
        "random id resolves to nothing"
    );
}

fn query(scopes: ScopeSet, seeds: Vec<NodeId>, max_hops: u8) -> TraversalQuery {
    TraversalQuery {
        scopes,
        seeds,
        max_hops,
        edge_types: None,
        direction: Direction::Out,
        as_of: None,
    }
}

#[test]
fn traversal_with_empty_seeds_is_empty_not_an_error() {
    let (db, _d, s, _id) = db_with_one_note("hello world");
    let sg = db.traverse(&query(ScopeSet::of(&[s]), vec![], 3)).unwrap();
    assert!(sg.nodes.is_empty(), "empty seeds => empty subgraph");
}

#[test]
fn traversal_max_hops_is_validated_to_its_documented_range() {
    let (db, _d, s, id) = db_with_one_note("hello world");
    let scopes = ScopeSet::of(&[s]);

    // Out of range below and above the documented 1..=4 window is Rejected,
    // not a panic and not silent clamping.
    for bad in [0u8, 5, 255] {
        let r = db.traverse(&query(scopes.clone(), vec![id], bad));
        assert!(
            matches!(&r, Err(TopoError::Rejected(_))),
            "max_hops={bad} must be Rejected, got {r:?}"
        );
    }
    // The in-range extremes both succeed on a real seed.
    for ok in [1u8, 4] {
        db.traverse(&query(scopes.clone(), vec![id], ok))
            .unwrap_or_else(|e| panic!("max_hops={ok} should be valid, got {e:?}"));
    }
}
