//! One-shot frozen v3 workload corpus, captured by v4 plan Task 6 Step 0
//! **before** `fts.rs`'s postings maintenance is rewired to the chunked v4
//! layout — the SP1 Task-7 Step-0 pattern (`generate_v2_workload.rs`'s doc
//! comment), reused here because after Task 6 lands, no build on this branch
//! can write single-row-per-term v3 postings again.
//!
//! NOTE: unlike `v2-workload.redb` (a genuine pre-v3 file), this fixture is
//! captured on a branch that has already dual-written the v4
//! `vectors`/`embedding_ref` tables (Tasks 3/5) alongside the still-
//! authoritative v3 tables, and `FORMAT_VERSION` still reads 3 at capture
//! time. So this is NOT a pure shipped-0.0.6 v3 file — it's a v3 file with
//! v4 dual-writes already present, which is fine for CONTENT-equivalence
//! testing (the migration corpus this fixture exists for) but must not be
//! mistaken for the pure-legacy case; Task 7 recovers a genuine
//! pre-v4-dual-write v3 fixture separately for that.
use topodb::workload::{batches, WorkloadSpec};
use topodb::{Db, IndexSpec, NodeId, Op, PropIndex};

/// Mirrors `workload::memory_id` (private to that module) exactly, so this
/// generator can target memory 0 — already given a `"bench-768"` embedding
/// by `batches` below (`embed_pct: 40` covers index 0) — for the follow-up
/// second-model re-embed without needing to export that helper.
fn memory_id(i: usize) -> NodeId {
    NodeId::from_u128(0x0100_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}

#[test]
#[ignore]
fn generate_v3_workload_fixture() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3-workload.redb");
    let _ = std::fs::remove_file(&path);
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
    // 200 memories drawn from workload.rs's 16-word vocabulary, 50-500 words
    // each: every word recurs across most of the 200 documents, so several
    // terms end up with rich multi-entry postings without any hand-written
    // text. embed_pct: 40 gives ~80 memories (including memory 0) a
    // "bench-768" embedding.
    for batch in batches(&WorkloadSpec {
        memories: 200,
        embed_pct: 40,
        ..Default::default()
    }) {
        db.submit(batch).unwrap();
    }
    // Second-model re-embed: memory 0 already carries a "bench-768"
    // embedding from the loop above; re-embedding it under a distinct model
    // name + dim exercises the model-change re-embed path (the old
    // (model, node) row is deleted, not accumulated) so Task 7's migration
    // corpus contains a genuine cross-model transition, not just
    // single-model rows.
    db.submit(vec![Op::SetEmbedding {
        id: memory_id(0),
        model: "bench-384".into(),
        vector: vec![0.25; 384],
    }])
    .unwrap();
}
