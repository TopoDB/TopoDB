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

/// F8 Task 4: proves the read path genuinely dispatches through the graph
/// for a built cluster (not silently scanning) — via the debug instrumentation
/// atomic — AND that results stay sane despite `ef=max(4*k,64)=64` covering
/// only part of a 200-vector cluster. Also covers the multi-scope merge case:
/// one built scope (A, 200 vectors -> graph) and one sub-threshold scope (B,
/// 5 vectors -> scan) queried together in a single call.
#[test]
fn built_cluster_uses_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny(&dir.path().join("t.redb"));

    // Scope A (Shared): 200 vectors on the unit circle -> crosses
    // build_threshold=8, so the cluster is built.
    let n = 200;
    let ids_a = seed(&db, "m1", n);
    let meta = only_meta_row(&db).expect("scope A must be built at 200 >= threshold 8");
    assert!(meta.3, "scope A cluster must be built");

    // Instrumentation pin: a built cluster's search must dispatch through
    // `hnsw::search`, not silently keep scanning. Fails before Task 4's
    // routing exists (the atomic is always false pre-routing).
    let _ = query_top_ids(&db, "m1", 10);
    assert!(
        db.debug_last_search_used_graph(),
        "a built cluster's search must route through the HNSW graph"
    );

    // k=10, ef=max(4*10,64)=64 covers only part of the 200-vector cluster,
    // so recall may be < 1.0 — assert conservatively: every returned id is
    // within the brute-force top-64 (not necessarily the exact top-10), and
    // exactly 10 ids come back.
    let mut brute: Vec<(usize, f32)> = (0..n)
        .map(|i| {
            let v = vec_at(i);
            (i, v[0]) // cosine(query=[1,0], v) == v[0] on the unit circle
        })
        .collect();
    brute.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let brute_top64: std::collections::HashSet<NodeId> =
        brute[..64].iter().map(|&(i, _)| ids_a[i]).collect();
    let got = query_top_ids(&db, "m1", 10);
    assert_eq!(got.len(), 10, "search must still return k results");
    for id in &got {
        assert!(
            brute_top64.contains(id),
            "every graph hit must be within the brute-force top-64"
        );
    }

    // Scope B: a second, sub-threshold scope under the SAME model. `vec_at`'s
    // theta range (0..19.9 rad) spans more than 3 full turns of the unit
    // circle, so A's cosine-vs-query distribution wraps around and clusters
    // densely near 1.0 at MULTIPLE points (near theta = 0, 2*pi, 4*pi, ...)
    // — NOT just near i=0 (the module doc comment's "strictly decreasing"
    // claim only holds for theta in [0, pi/2), which 200 points blow past).
    // So B's vectors can't be hand-picked to safely beat "A's first few"; they
    // must beat A's TRUE 6th-highest cosine score computed by brute force
    // over the actual 200 A vectors, whatever direction those particular 6
    // happen to be at. Then, with B's worst score `> a_rank6`, at most 5 of
    // A's real candidates (its true top 5) can ever outscore any B entry —
    // guaranteeing all 5 of B's ids survive into the merged top-10 regardless
    // of which subset of A's true top-64 the graph's approximate search
    // happens to surface (every surfaced A score is an exact cosine of a
    // real vector, so "beats B" is a real, not approximate, fact).
    let a_true_scores: Vec<f32> = (0..n).map(|i| vec_at(i)[0]).collect();
    let mut a_sorted = a_true_scores.clone();
    a_sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let a_rank6 = a_sorted[5]; // 6th-highest (0-indexed 5) true score in A.

    let scope_b_id = ScopeId::new();
    let mut ids_b = Vec::new();
    for i in 0..5 {
        let id = NodeId::new();
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(scope_b_id),
            label: "M".into(),
            props: Default::default(),
        }])
        .unwrap();
        // Vanishingly small theta -> cosine within a few ULPs of 1.0, well
        // clear of `a_rank6` (checked below) regardless of A's own near-1.0
        // wraparound points.
        let theta = (i as f32 + 1.0) * 1e-5;
        let vector = vec![theta.cos(), theta.sin()];
        assert!(
            vector[0] > a_rank6,
            "test construction invariant: B's score must clear A's true 6th-best"
        );
        db.submit(vec![Op::SetEmbedding {
            id,
            model: "m1".into(),
            vector,
        }])
        .unwrap();
        ids_b.push(id);
    }

    // Scope B alone: well under build_threshold=8 -> must stay scan-routed.
    let got_b = db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[scope_b_id]),
            model: "m1".into(),
            vector: vec![1.0, 0.0],
            k: 5,
            candidates: None,
        })
        .unwrap();
    assert!(
        !db.debug_last_search_used_graph(),
        "a sub-threshold scope's search must NOT route through the graph"
    );
    assert_eq!(got_b.len(), 5);

    // Multi-scope merge: query A (graph) and B (scan) together in ONE call.
    // By construction (see above), at most 5 of A's real candidates can
    // outscore any of B's 5, so the merged top-10 must contain every one of
    // B's ids — assert exactly that documented tolerance, plus `len == k`
    // and score-descending order, rather than the full exact composition
    // (which would be sensitive to any graph inexactness on A's side).
    let merged = db
        .search_vector(&VectorQuery {
            scopes: ScopeSet::of(&[scope_b_id]).with_shared(),
            model: "m1".into(),
            vector: vec![1.0, 0.0],
            k: 10,
            candidates: None,
        })
        .unwrap();
    assert_eq!(merged.len(), 10, "merged query must still return k results");
    for pair in merged.windows(2) {
        assert!(
            pair[0].1 >= pair[1].1,
            "merged results must be sorted score-descending"
        );
    }
    let merged_ids: std::collections::HashSet<NodeId> =
        merged.iter().map(|(rec, _)| rec.id).collect();
    for id in &ids_b {
        assert!(
            merged_ids.contains(id),
            "scope B's results must all be present in the merged query"
        );
    }
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

/// F8 Task 4 regression pin: `search_layer` must be able to route THROUGH a
/// tombstoned slot as a pure waypoint (its `neighbors` list, not its score),
/// most acutely when the tombstoned slot is the cluster's own
/// `entry_slot` — every read-path search starts its greedy descent there
/// (`hnsw::search`), so a stale/dangling entry is the single likeliest way
/// for a revert of that fix to silently break search.
///
/// Deterministic by construction, unlike
/// `remove_node_tombstones_and_ratio_triggers_rebuild` (which seeds with
/// random `NodeId::new()`, so its removed victims only SOMETIMES land on
/// `entry_slot`): `Op::CreateNode`'s handler (`storage.rs`) allocates node
/// slots via `alloc_node_slot` strictly in submission order starting at 0
/// (see `slots.rs`), and this test's `(model, scope)` cluster is otherwise
/// empty, so seeding node `i` (0-indexed) always gives it slot `i` exactly.
/// Fixed `NodeId::from_u128` ids (mirroring the determinism idiom
/// `hnsw.rs`'s own inline tests use — see `alloc_node_slot(... ids[slot])`
/// there) make that slot assignment, and therefore which id ends up at
/// `entry_slot`, fully reproducible run to run — no extra `src` dump helper
/// needed since `entry_slot -> id` is just `ids[entry_slot as usize]`.
#[test]
fn removing_the_entry_point_node_keeps_search_correct() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_tiny(&dir.path().join("t.redb"));

    let n = 12usize;
    let ids: Vec<NodeId> = (0..n)
        .map(|i| NodeId::from_u128(9000 + i as u128))
        .collect();
    for (i, &id) in ids.iter().enumerate() {
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
    }

    let meta0 = only_meta_row(&db).expect("must be built: 12 >= threshold 8");
    assert!(meta0.3, "cluster must be built");
    let (model, scope) = (meta0.0, meta0.1);

    // Tracks (id, original index) so the brute-force order (ascending
    // index — per the module doc comment, strictly decreasing cosine for
    // `i * 0.1` in `[0, pi/2)`, and `11 * 0.1 = 1.1 < pi/2`) can be
    // recomputed after each removal.
    let mut remaining: Vec<(NodeId, usize)> = ids.iter().copied().zip(0..n).collect();
    let brute_force_ids = |remaining: &[(NodeId, usize)]| -> Vec<NodeId> {
        let mut sorted = remaining.to_vec();
        sorted.sort_by_key(|&(_, i)| i);
        sorted.into_iter().map(|(id, _)| id).collect()
    };
    let link_tomb = |db: &Db, slot: u64| -> bool {
        db.debug_dump_hnsw_links()
            .unwrap()
            .into_iter()
            .find(|r| r.0 == model && r.1 == scope && r.2 == slot && r.3 == 0)
            .map(|r| r.4)
            .expect("level-0 link row must exist for every ever-inserted slot")
    };

    // --- Removal 1: the cluster's ACTUAL entry point. ---
    let entry_slot_1 = meta0.4;
    assert!(
        !link_tomb(&db, entry_slot_1),
        "sanity: entry slot must not already be tombstoned before any removal"
    );
    let victim_1 = ids[entry_slot_1 as usize];
    db.submit(vec![Op::RemoveNode { id: victim_1 }]).unwrap();
    remaining.retain(|&(id, _)| id != victim_1);

    // Independent proof — not just re-use of the same index arithmetic —
    // that `victim_1` really was the node occupying `entry_slot_1`:
    // removing it must be exactly what flips THAT slot's level-0 link row
    // tombstoned.
    assert!(
        link_tomb(&db, entry_slot_1),
        "removing victim_1 must tombstone entry_slot_1's own link row"
    );

    // `hnsw::tombstone` only flips a slot's `tomb` bit and bumps
    // `meta.stale`; it never rewrites `meta.entry_slot` (only a full
    // rebuild does, via `maybe_rebuild_cluster`). With ratio defaults
    // (rebuild_num/den = 3/10) and graph_len=12, one tombstone (stale=1) is
    // nowhere near the rebuild threshold (stale*10 >= 12*3=36 needs
    // stale>=4), so `entry_slot` is now a genuinely dangling, permanently
    // dead waypoint — exactly the case the Task 4 fix must route through.
    let meta1 = only_meta_row(&db).expect("cluster stays built");
    assert!(meta1.3);
    assert_eq!(
        meta1.4, entry_slot_1,
        "no rebuild yet: entry_slot must still point at the tombstoned slot"
    );
    assert_eq!(meta1.7, 1, "exactly one tombstone so far");

    // Search must still work: brute-force top-k over the 11 survivors
    // (default ef=128 covers all of them, so this is exact-order), and
    // victim_1 must never appear.
    let want1 = brute_force_ids(&remaining);
    let got1 = query_top_ids(&db, "m1", remaining.len());
    assert_eq!(
        got1, want1,
        "search through a tombstoned ENTRY POINT must still return the exact brute-force order"
    );
    assert!(!got1.contains(&victim_1));

    // --- Removal 2: entry_slot_1's own first-hop routing neighbor. ---
    // `entry_slot` never moves off a tombstoned slot without a rebuild
    // (just established above), so there is no "new entry_slot value" to
    // read back from `debug_dump_hnsw_meta` a second time. The genuine way
    // to double up the dead-waypoint chain a correct read path must descend
    // through is to also kill whichever slot search would hop to FIRST from
    // the still-dead entry — one of entry_slot_1's own level-0 neighbors
    // (`tombstone` never touches `neighbors`, only the `tomb` flag, so that
    // list is still intact).
    let entry_neighbors = db
        .debug_dump_hnsw_links()
        .unwrap()
        .into_iter()
        .find(|r| r.0 == model && r.1 == scope && r.2 == entry_slot_1 && r.3 == 0)
        .expect("entry_slot_1's level-0 row must still exist")
        .5;
    let next_hop_slot = *entry_neighbors
        .iter()
        .find(|&&s| s != entry_slot_1)
        .expect("a 12-node cluster's entry must have at least one other neighbor");
    assert!(
        !link_tomb(&db, next_hop_slot),
        "sanity: the chosen second victim's slot must not already be tombstoned"
    );
    let victim_2 = ids[next_hop_slot as usize];
    assert_ne!(victim_2, victim_1, "must be a genuinely different node");

    db.submit(vec![Op::RemoveNode { id: victim_2 }]).unwrap();
    remaining.retain(|&(id, _)| id != victim_2);

    assert!(
        link_tomb(&db, next_hop_slot),
        "removing victim_2 must tombstone its own link row"
    );
    let meta2 = only_meta_row(&db).expect("cluster stays built");
    assert!(meta2.3);
    assert_eq!(
        meta2.4, entry_slot_1,
        "still no rebuild: entry_slot pins to the same permanently-dead slot"
    );
    assert_eq!(
        meta2.7, 2,
        "two tombstones now, still short of the ratio's rebuild trigger"
    );

    // Search must still work through BOTH dead waypoints: the entry itself
    // AND its first live-routing hop are now tombstoned, so a correct read
    // path must keep descending past both to reach the 10 live survivors.
    let want2 = brute_force_ids(&remaining);
    let got2 = query_top_ids(&db, "m1", remaining.len());
    assert_eq!(
        got2, want2,
        "search must still return the exact brute-force order after a SECOND \
         consecutive entry-point-chain removal"
    );
    assert!(!got2.contains(&victim_1));
    assert!(!got2.contains(&victim_2));
}
