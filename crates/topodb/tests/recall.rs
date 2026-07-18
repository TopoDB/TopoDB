//! Behavioral tests for Db::recall — the production hybrid fusion API.
use topodb::*;

fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    }
}

fn memory(content: &str, scope: Scope) -> (NodeId, Op) {
    let id = NodeId::new();
    let mut props = Props::new();
    props.insert("content".into(), PropValue::Str(content.into()));
    (
        id,
        Op::CreateNode {
            id,
            scope,
            label: "Memory".into(),
            props,
        },
    )
}

fn text_only(scopes: &ScopeSet, query: &str, k: usize) -> RecallQuery {
    RecallQuery {
        scopes: scopes.clone(),
        query: query.into(),
        k,
        vector: None,
        expansions: vec![],
        graph_boost: false,
        options: SearchOptions::default(),
    }
}

#[test]
fn text_only_recall_orders_like_search_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    let (_a, op_a) = memory("rust embedded database engine", Scope::Id(s));
    let (_b, op_b) = memory("rust gardening tips", Scope::Id(s));
    let (_c, op_c) = memory("cooking with rust free pans", Scope::Id(s));
    db.submit(vec![op_a, op_b, op_c]).unwrap();

    let bm25: Vec<NodeId> = db
        .search_text(&scopes, "rust database", 10)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    let fused: Vec<NodeId> = db
        .recall(&text_only(&scopes, "rust database", 10))
        .unwrap()
        .into_iter()
        .map(|(n, _)| n.id)
        .collect();
    assert_eq!(fused, bm25, "single-leg recall must preserve BM25 order");
}

#[test]
fn recall_truncates_to_k_and_validates_input() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
    let s = ScopeId::new();
    let scopes = ScopeSet::of(&[s]);
    for i in 0..5 {
        let (_x, op) = memory(&format!("common token filler {i}"), Scope::Id(s));
        db.submit(vec![op]).unwrap();
    }
    assert_eq!(
        db.recall(&text_only(&scopes, "common", 2)).unwrap().len(),
        2
    );

    // k == 0 and token-less query reject exactly like search_text.
    assert!(matches!(
        db.recall(&text_only(&scopes, "common", 0)),
        Err(TopoError::Rejected(_))
    ));
    assert!(matches!(
        db.recall(&text_only(&scopes, "!!!", 10)),
        Err(TopoError::Rejected(_))
    ));
    // Empty query vector is a host bug — loud, not a silent skipped leg.
    let mut q = text_only(&scopes, "common", 5);
    q.vector = Some(("m".into(), vec![]));
    assert!(matches!(db.recall(&q), Err(TopoError::Rejected(_))));
}
