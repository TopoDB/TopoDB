//! Consumes the frozen v3 workload corpus (`generate_v3_workload.rs`'s
//! `#[ignore]`d one-shot generator) as a migration-correctness fixture for
//! Task 7 (format v4): opening it must migrate all the way from
//! `FORMAT_VERSION == 3` to `4`, re-chunking its single-row-per-term
//! POSTINGS and leaving its dual-written `vectors`/`embedding_ref` rows
//! intact — verified by BOTH `search_text` and `search_vector` still
//! returning the expected hits, matching the query-result contract
//! `format_fixture.rs`'s smaller fixtures pin.
use topodb::{Db, IndexSpec, NodeId, PropIndex, ScopeId, ScopeSet, VectorQuery};

/// Mirrors `workload::memory_id` (private to that module) exactly — see
/// `generate_v3_workload.rs`'s identical helper.
fn memory_id(i: usize) -> NodeId {
    NodeId::from_u128(0x0100_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}

#[test]
fn v3_workload_fixture_migrates_to_v4_and_reads() {
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3-workload.redb");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workload.redb");
    std::fs::copy(source, &path).unwrap(); // never open the committed file read-write

    let spec = IndexSpec {
        equality: vec![PropIndex {
            label: "Entity".into(),
            prop: "name".into(),
        }],
        text: vec![PropIndex {
            label: "Memory".into(),
            prop: "content".into(),
        }],
    };
    let db = Db::open_with(&path, spec).unwrap();
    assert_eq!(db.format_version(), 5);

    let scopes = ScopeSet::of(&[ScopeId::from_u128(1)]);

    // search_text: every one of the 16 vocabulary words recurs across most
    // of the 200 documents (see the generator's doc comment) — any of them
    // must hit.
    assert!(
        !db.search_text(&scopes, "agent memory", 10)
            .unwrap()
            .is_empty(),
        "text search must find hits in the migrated postings"
    );

    // search_vector, model "bench-768": memories 1..79 kept their original
    // embedding (embed_pct: 40 of 200 memories = indices 0..79; memory 0 was
    // re-embedded below to a different model). A query vector matching
    // memory 1's exact original vector is unknown (workload-generated, not
    // reproduced here), but a plain probe vector must still surface SOME
    // hits from that ~79-row cluster.
    let hits = db
        .search_vector(&VectorQuery {
            scopes: scopes.clone(),
            model: "bench-768".into(),
            vector: vec![0.1; 768],
            k: 10,
            candidates: None,
        })
        .unwrap();
    assert!(
        !hits.is_empty(),
        "vector search under the original model must find migrated embeddings"
    );
    assert!(
        hits.iter().all(|(n, _)| n.id != memory_id(0)),
        "memory 0 was re-embedded to a different model; it must not appear under bench-768"
    );

    // search_vector, model "bench-384": memory 0's cross-model re-embed
    // (the generator's whole point — proving the migration corpus contains
    // a genuine cross-model transition). The query is memory 0's EXACT
    // vector (a constant, per the generator), so this is an exact match:
    // memory 0 must be the (only) hit.
    let exact = db
        .search_vector(&VectorQuery {
            scopes: scopes.clone(),
            model: "bench-384".into(),
            vector: vec![0.25; 384],
            k: 1,
            candidates: None,
        })
        .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].0.id, memory_id(0));
}
