//! Integration test for the "host-style" recall pattern: traverse a
//! neighborhood for candidates, rank those candidates by vector similarity,
//! separately rank the whole scope by BM25 text relevance, then fuse the two
//! ranked lists with reciprocal-rank fusion (RRF) — exactly what a host
//! would do to combine structural, semantic, and lexical recall. No new
//! production API is exercised here; this is belt-and-suspenders coverage
//! that `traverse` → `search_vector`(`candidates`) → `search_text` → RRF
//! compose correctly, and that reads still bump `access_stats` along the way.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use topodb::*;

/// Same pattern as `counters.rs`'s helper: bumps are async (batched
/// ~100ms), so poll with a deadline instead of sleeping blind.
fn wait_for_count(db: &Db, scopes: &ScopeSet, id: NodeId, want_at_least: u64) -> AccessStats {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(stats) = db.access_stats(scopes, id).unwrap() {
            if stats.access_count >= want_at_least {
                return stats;
            }
        }
        assert!(
            Instant::now() < deadline,
            "counter never reached {want_at_least}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Reciprocal-rank fusion over two id-ranked lists (1-based rank), the way a
/// host would merge a vector-search ranking with a text-search ranking. A
/// node absent from a list simply contributes nothing from that list.
/// `k = 60` is the standard RRF damping constant.
fn rrf_merge(text_ranked: &[NodeId], vector_ranked: &[NodeId]) -> Vec<(NodeId, f64)> {
    const K: f64 = 60.0;
    let mut scores: HashMap<NodeId, f64> = HashMap::new();
    for (i, id) in text_ranked.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += 1.0 / (K + (i + 1) as f64);
    }
    for (i, id) in vector_ranked.iter().enumerate() {
        *scores.entry(*id).or_insert(0.0) += 1.0 / (K + (i + 1) as f64);
    }
    let mut merged: Vec<(NodeId, f64)> = scores.into_iter().collect();
    merged.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    merged
}

/// 20 "Memory" nodes in one scope: a hub (the traversal seed) with 14 direct
/// neighbors (the traversable candidate pool) plus 5 unconnected decoys
/// (present only in the corpus, never in the traversal subgraph). Node 1
/// ("BEST") is deliberately unambiguous in both modalities:
/// - **text**: it is the only node containing all four query terms
///   ("rust", "embedded", "database", "engine") — every other node in the
///   candidate pool contains at most one of them, and the decoys contain
///   none;
/// - **vector**: the query vector is its embedding *exactly*, so its cosine
///   score is the maximum possible (1.0); no other vector here is a scalar
///   multiple of it, so no other node can tie.
struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    scope: Scope,
    scopes: ScopeSet,
    hub: NodeId,
    best: NodeId,
    query_vector: Vec<f32>,
}

const QUERY_TEXT: &str = "rust embedded database engine";

fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let spec = IndexSpec {
        equality: vec![],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(dir.path().join("t.redb"), spec).unwrap();
    let s = ScopeId::new();
    let scope = Scope::Id(s);
    let scopes = ScopeSet::of(&[s]);

    let ids: Vec<NodeId> = (0..20).map(|_| NodeId::new()).collect();
    let hub = ids[0];
    let best = ids[1];

    let contents: [&str; 20] = [
        "hub anchor node placeholder text", // 0: hub (seed)
        "rust embedded database engine",    // 1: BEST — all 4 query terms
        "rust programming language basics", // 2: partial text overlap (rust only)
        "gardening tips for spring bloom",  // 3: vector-only rival, no text overlap
        "javascript frontend framework tutorial",
        "cooking recipes for weeknight dinners",
        "hiking trails in the pacific northwest",
        "photography lighting techniques for portraits",
        "chess opening theory for beginners",
        "woodworking joinery techniques explained",
        "astronomy observing deep sky objects",
        "baking sourdough bread at home",
        "cycling routes through mountain passes",
        "birdwatching migration patterns in autumn",
        "pottery glazing techniques for stoneware",
        "unreached decoy about topics that never overlap",
        "unreached decoy about entirely different things",
        "unreached decoy with no shared vocabulary at all",
        "unreached decoy discussing something unrelated",
        "unreached decoy covering yet another subject",
    ];

    // 4-dim embeddings, one per node above (same index correspondence).
    // None of indices 2..20 is a scalar multiple of vectors[1] — verified by
    // inspection (differing component ratios) — so node 1 is the unique
    // cosine-similarity maximum against the query (which equals vectors[1]
    // exactly).
    let vectors: [[f32; 4]; 20] = [
        [0.1, 0.1, 0.1, 0.1],    // 0 hub
        [1.0, 0.5, 0.25, 0.125], // 1 BEST
        [0.0, 1.0, 0.0, 0.0],
        [0.8, 0.6, 0.0, 0.0],
        [0.2, 0.9, 0.1, 0.0],
        [0.3, 0.1, 0.9, 0.2],
        [0.4, 0.2, 0.1, 0.9],
        [0.9, 0.1, 0.4, 0.2],
        [0.1, 0.9, 0.3, 0.1],
        [0.5, 0.5, 0.1, 0.1],
        [0.2, 0.3, 0.8, 0.1],
        [0.6, 0.1, 0.2, 0.7],
        [0.7, 0.3, 0.1, 0.5],
        [0.1, 0.6, 0.5, 0.2],
        [0.3, 0.4, 0.2, 0.8],
        [0.05, 0.05, 0.05, 0.05],
        [0.02, 0.03, 0.04, 0.05],
        [0.9, 0.05, 0.02, 0.01],
        [0.15, 0.25, 0.35, 0.45],
        [0.4, 0.1, 0.3, 0.2],
    ];

    let mut create_ops = Vec::new();
    for i in 0..20 {
        let mut props = Props::new();
        props.insert("content".into(), PropValue::Str(contents[i].into()));
        create_ops.push(Op::CreateNode {
            id: ids[i],
            scope,
            label: "Memory".into(),
            props,
        });
    }
    db.submit(create_ops).unwrap();

    let mut embed_ops = Vec::new();
    for i in 0..20 {
        embed_ops.push(Op::SetEmbedding {
            id: ids[i],
            model: "m1".into(),
            vector: vectors[i].to_vec(),
        });
    }
    db.submit(embed_ops).unwrap();

    // Hub -> its 14 direct neighbors (ids[1..15]) — the traversable
    // candidate pool; ids[15..20] stay unreachable decoys.
    let mut edge_ops = Vec::new();
    for &to in &ids[1..15] {
        edge_ops.push(Op::CreateEdge {
            id: EdgeId::new(),
            scope,
            ty: "RELATES_TO".into(),
            from: hub,
            to,
            props: Default::default(),
            valid_from: None,
        });
    }
    db.submit(edge_ops).unwrap();

    Fixture {
        _dir: dir,
        db,
        scope,
        scopes,
        hub,
        best,
        query_vector: vectors[1].to_vec(),
    }
}

#[test]
fn traverse_then_vector_and_text_recall_fuse_with_best_match_first() {
    let f = build_fixture();

    // Host step 1: traverse from the seed to get a candidate neighborhood.
    let sub =
        f.db.traverse(&TraversalQuery {
            scopes: f.scopes.clone(),
            seeds: vec![f.hub],
            max_hops: 1,
            edge_types: None,
            direction: Direction::Out,
            as_of: None,
        })
        .unwrap();
    let candidate_ids: Vec<NodeId> = sub.nodes.iter().map(|n| n.id).collect();
    assert_eq!(candidate_ids.len(), 15, "hub + its 14 direct neighbors");
    assert!(candidate_ids.contains(&f.best));

    // Host step 2: vector search restricted to the traversal candidates.
    let vec_hits =
        f.db.search_vector(&VectorQuery {
            scopes: f.scopes.clone(),
            model: "m1".into(),
            vector: f.query_vector.clone(),
            k: candidate_ids.len(),
            candidates: Some(candidate_ids.clone()),
        })
        .unwrap();
    let vector_ranked: Vec<NodeId> = vec_hits.iter().map(|(n, _)| n.id).collect();
    assert_eq!(
        vector_ranked.first(),
        Some(&f.best),
        "exact embedding match must rank first"
    );

    // Host step 3: text search over the whole scope (not candidate-restricted
    // — `search_text` has no `candidates` parameter, matching the real API).
    let text_hits = f.db.search_text(&f.scopes, QUERY_TEXT, 20).unwrap();
    let text_ranked: Vec<NodeId> = text_hits.iter().map(|(n, _)| n.id).collect();
    assert_eq!(
        text_ranked.first(),
        Some(&f.best),
        "the only node covering all 4 query terms must rank first"
    );

    // Host step 4: reciprocal-rank fusion, computed in the test body (no
    // production fusion API exists — this mirrors what a host implements).
    let fused = rrf_merge(&text_ranked, &vector_ranked);
    assert_eq!(
        fused.first().map(|(id, _)| *id),
        Some(f.best),
        "the dual-modality match must win the fused ranking, fused={fused:?}"
    );

    // Every node either search path returned must actually be in the queried
    // scope — a regression here would mean a scope filter got bypassed.
    for (rec, _) in vec_hits.iter().chain(text_hits.iter()) {
        assert_eq!(
            rec.scope, f.scope,
            "every returned node must be in the queried scope"
        );
    }

    // The three reads above (traverse, search_vector, search_text) each
    // bump `best`'s access counter once; bumps are async, so poll.
    let stats = wait_for_count(&f.db, &f.scopes, f.best, 3);
    assert!(stats.last_accessed_at > 0);
}
