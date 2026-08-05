//! HNSW lifecycle through the public Db API, with a tiny build threshold so
//! graphs activate on small corpora (F8 Task 3). Mirrors `vector_search.rs`'s
//! `db_with` idiom, adapted to open with `DbOptions::hnsw_params`.
//!
//! Every embedding used here lives on the unit circle: `vector(i) =
//! [cos(i * 0.1), sin(i * 0.1)]`, queried against `[1.0, 0.0]` (`theta = 0`).
//! Since both the query and every embedding are unit vectors, `cosine(query,
//! vector(i)) == cos(i * 0.1)` exactly (the norm-normalizing denominator is
//! 1), which is strictly decreasing for `i * 0.1` in `[0, pi/2)` — so the
//! brute-force top-k order is simply ascending `i`. That makes every
//! assertion below exact-order, hand-verifiable arithmetic instead of a
//! separate brute-force oracle.
use topodb::*;

fn vec_at(i: usize) -> Vec<f32> {
    let theta = i as f32 * 0.1;
    vec![theta.cos(), theta.sin()]
}

fn tiny_params() -> HnswParams {
    HnswParams {
        build_threshold: 8,
        ..Default::default()
    }
}

fn open_tiny(path: &std::path::Path) -> Db {
    Db::open_with_options(
        path,
        IndexSpec::default(),
        DbOptions {
            hnsw_params: Some(tiny_params()),
            ..Default::default()
        },
    )
    .unwrap()
}

fn open_default(path: &std::path::Path) -> Db {
    Db::open_with_options(path, IndexSpec::default(), DbOptions::default()).unwrap()
}

/// Creates `n` nodes, all `Scope::Shared`, embedding node `i` with
/// `vec_at(i)` under `model`, in ascending `i` order. Returns the ids in
/// that same creation order.
fn seed(db: &Db, model: &str, n: usize) -> Vec<NodeId> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = NodeId::new();
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Shared,
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
        db.submit(vec![Op::SetEmbedding {
            id,
            model: model.into(),
            vector: vec_at(i),
        }])
        .unwrap();
        ids.push(id);
    }
    ids
}

fn query_top_ids(db: &Db, model: &str, k: usize) -> Vec<NodeId> {
    query_top_ids_with(db, model, &[1.0, 0.0], k)
}

fn query_top_ids_with(db: &Db, model: &str, query: &[f32], k: usize) -> Vec<NodeId> {
    db.search_vector(&VectorQuery {
        scopes: ScopeSet::of(&[]).with_shared(),
        model: model.into(),
        vector: query.to_vec(),
        k,
        candidates: None,
    })
    .unwrap()
    .into_iter()
    .map(|(rec, _)| rec.id)
    .collect()
}

/// The single `(model, scope)` cluster's meta row, if the dump has exactly
/// one (every test here uses one model + the shared scope, so "the" meta row
/// is unambiguous once present). `(model, scope, format, built, entry_slot,
/// entry_level, graph_len, stale)` — matches `Db::debug_dump_hnsw_meta`'s row
/// shape exactly.
type MetaRow = (u32, u32, u8, bool, u64, u8, u64, u64);

fn only_meta_row(db: &Db) -> Option<MetaRow> {
    let rows = db.debug_dump_hnsw_meta().unwrap();
    assert!(rows.len() <= 1, "test fixtures use exactly one cluster");
    rows.into_iter().next()
}

#[test]
fn graph_builds_when_cluster_crosses_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny(&dir.path().join("t.redb"));

    let mut ids = Vec::new();
    for i in 0..7 {
        let id = NodeId::new();
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Shared,
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
        db.submit(vec![Op::SetEmbedding {
            id,
            model: "m1".into(),
            vector: vec_at(i),
        }])
        .unwrap();
        ids.push(id);
    }
    assert!(
        db.debug_dump_hnsw_meta().unwrap().is_empty(),
        "below build_threshold=8: no meta row yet"
    );
    assert!(
        db.debug_dump_hnsw_links().unwrap().is_empty(),
        "below build_threshold=8: no link rows yet"
    );

    // 8th embedding crosses the threshold.
    let id8 = NodeId::new();
    db.submit(vec![Op::CreateNode {
        id: id8,
        scope: Scope::Shared,
        label: "M".into(),
        props: Default::default(),
    }])
    .unwrap();
    db.submit(vec![Op::SetEmbedding {
        id: id8,
        model: "m1".into(),
        vector: vec_at(7),
    }])
    .unwrap();
    ids.push(id8);

    let (_model, _scope, format, built, _entry_slot, _entry_level, graph_len, stale) =
        only_meta_row(&db).expect("cluster must be built at exactly 8 embeddings");
    assert_eq!(format, 0);
    assert!(built);
    assert_eq!(graph_len, 8);
    assert_eq!(stale, 0);

    // ef (default 128) covers all 8 -> exact brute-force order (ascending i,
    // per the module doc comment above).
    let got = query_top_ids(&db, "m1", 8);
    assert_eq!(got, ids, "k=8 must return the exact brute-force order");
}

#[test]
fn below_threshold_results_identical_to_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_default(&dir.path().join("t.redb")); // default threshold = 1024
    let ids = seed(&db, "m1", 6);

    assert!(
        db.debug_dump_hnsw_meta().unwrap().is_empty(),
        "6 embeddings must stay well below the default 1024 threshold"
    );
    assert!(db.debug_dump_hnsw_links().unwrap().is_empty());

    let got = query_top_ids(&db, "m1", 6);
    assert_eq!(
        got, ids,
        "scan-only search must equal the hand-computed brute-force order"
    );
}

#[test]
fn remove_node_tombstones_and_ratio_triggers_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny(&dir.path().join("t.redb"));
    let ids = seed(&db, "m1", 10); // crosses build_threshold=8 on the 8th; 9,10 insert into the built graph.

    let (_m, _s, _f, built, _es, _el, graph_len, stale) =
        only_meta_row(&db).expect("must be built after 10 embeddings");
    assert!(built);
    assert_eq!(graph_len, 10);
    assert_eq!(stale, 0);

    // Remove 3 nodes one at a time; ratio = rebuild_num/rebuild_den =
    // 3/10 (defaults). Boundary: stale*10 >= graph_len*3.
    // stale=1: 10 >= 30? no.  stale=2: 20 >= 30? no.  stale=3: 30 >= 30? yes.
    let mut remaining = ids.clone();
    for (removed_count, &victim) in ids[0..3].iter().enumerate() {
        db.submit(vec![Op::RemoveNode { id: victim }]).unwrap();
        remaining.retain(|&id| id != victim);

        let got = query_top_ids(&db, "m1", 10);
        assert!(
            !got.contains(&victim),
            "a removed node must never appear in results"
        );
        for id in &remaining {
            assert!(got.contains(id), "every surviving node must still be found");
        }

        let (_m, _s, _f, built, _es, _el, graph_len, stale) =
            only_meta_row(&db).expect("cluster stays built across tombstones");
        assert!(built);
        match removed_count {
            0 => {
                assert_eq!(stale, 1);
                assert_eq!(graph_len, 10, "no rebuild yet: graph_len untouched");
            }
            1 => {
                assert_eq!(
                    stale, 2,
                    "ratio 20*1 >= 30 is false: must NOT have rebuilt yet"
                );
                assert_eq!(graph_len, 10);
            }
            2 => {
                assert_eq!(
                    stale, 0,
                    "ratio 30 >= 30 fires exactly at the 3rd removal: rebuilt, stale reset"
                );
                assert_eq!(
                    graph_len, 7,
                    "rebuild re-derives graph_len from the 7 currently-live vectors"
                );
            }
            _ => unreachable!(),
        }
    }

    // Final sanity: search still returns exactly the 7 survivors, in the
    // hand-computed brute-force order restricted to them.
    let got = query_top_ids(&db, "m1", 10);
    let want: Vec<NodeId> = remaining;
    assert_eq!(got, want);
}

#[test]
fn same_model_reembed_rewires_and_counts_stale() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny(&dir.path().join("t.redb"));
    let ids = seed(&db, "m1", 8); // exactly crosses threshold on the 8th.

    let before = only_meta_row(&db).expect("built at 8");
    assert_eq!(before.6, 8); // graph_len
    assert_eq!(before.7, 0); // stale

    // Re-embed the LAST node (originally the worst-ranked, theta=0.7) with a
    // vector that uniquely maximizes cosine against a query NOT already
    // tied by any seeded node (seeded thetas are 0.0..=0.7; -0.5 isn't one
    // of them), so it must become the unambiguous new top-1.
    let target = ids[7];
    let boosted = vec![(-0.5f32).cos(), (-0.5f32).sin()];
    db.submit(vec![Op::SetEmbedding {
        id: target,
        model: "m1".into(),
        vector: boosted.clone(),
    }])
    .unwrap();

    let got = query_top_ids_with(&db, "m1", &boosted, 1);
    assert_eq!(
        got[0], target,
        "search must reflect the re-embedded vector, not the stale one"
    );

    let after = only_meta_row(&db).expect("still built");
    assert_eq!(
        after.7,
        before.7 + 1,
        "reinsert_links must bump stale by exactly 1"
    );
    assert_eq!(
        after.6, before.6,
        "a same-cluster rewire must never touch graph_len"
    );
}

#[test]
fn cross_model_reembed_moves_clusters() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny(&dir.path().join("t.redb"));
    let ids = seed(&db, "m1", 8); // m1 cluster crosses threshold=8 and builds.

    let m1_before = only_meta_row(&db).expect("m1 built at 8");
    assert_eq!(m1_before.6, 8);
    assert_eq!(m1_before.7, 0);

    let target = ids[0];
    db.submit(vec![Op::SetEmbedding {
        id: target,
        model: "m2".into(),
        vector: vec![1.0, 0.0],
    }])
    .unwrap();

    // m1: still built, that slot's row newly tombstoned -> stale +1. Ratio
    // 1*10=10 >= 8*3=24 is false, so it must NOT have rebuilt.
    let m1_meta = db
        .debug_dump_hnsw_meta()
        .unwrap()
        .into_iter()
        .find(|r| r.0 == m1_before.0 && r.1 == m1_before.1)
        .expect("m1 cluster still present");
    assert!(m1_meta.3, "m1 cluster stays built");
    assert_eq!(m1_meta.7, 1, "m1's stale must bump by 1 from the tombstone");
    assert_eq!(m1_meta.6, 8, "m1's graph_len untouched by a tombstone");

    let m1_links = db.debug_dump_hnsw_links().unwrap();
    let tomb_count = m1_links
        .iter()
        .filter(|(model, scope, _slot, level, tomb, _nbrs)| {
            *model == m1_before.0 && *scope == m1_before.1 && *level == 0 && *tomb
        })
        .count();
    assert_eq!(tomb_count, 1, "exactly one level-0 row must be tombstoned");

    // m1 no longer finds the moved node.
    let m1_hits = query_top_ids(&db, "m1", 8);
    assert!(!m1_hits.contains(&target));

    // m2: only 1 vector, well under threshold=8 -> stays scan (no meta row).
    let m2_meta_rows: Vec<_> = db
        .debug_dump_hnsw_meta()
        .unwrap()
        .into_iter()
        .filter(|r| r.0 != m1_before.0 || r.1 != m1_before.1)
        .collect();
    assert!(
        m2_meta_rows.is_empty(),
        "m2 cluster has only 1 vector; must stay unbuilt below threshold"
    );

    // m2 finds it via the (still scan-based, Task 4 territory) read path.
    let m2_hits = query_top_ids(&db, "m2", 1);
    assert_eq!(m2_hits, vec![target]);
}
