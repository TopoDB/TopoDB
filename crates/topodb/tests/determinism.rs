use proptest::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use topodb::*;
use topodb::graph::AdjEntry;

/// Fixed, distinct vocabulary for `Intent::SetText` — deterministic (no
/// random strings), so BM25 postings are reproducible across replay.
const WORDS: [&str; 8] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];

/// Generate a small random-but-valid op sequence: create nodes, then a mix
/// of edges/prop-sets/closes/removes/text-sets referencing only created ids.
/// Abstract intents, lowered to concrete Ops inside the test where real ids
/// exist. Indices are taken modulo the live collection length, so every
/// generated script is valid by construction.
#[derive(Debug, Clone)]
enum Intent {
    Edge { from_ix: usize, to_ix: usize },
    Close { edge_ix: usize },
    SetProp { node_ix: usize, val: i64 },
    /// Sets the declared text prop (`"text"`) to a fixed word from `WORDS`
    /// (chosen via `word_ix % WORDS.len()`), exercising the FTS postings
    /// maintenance path under replay.
    SetText { node_ix: usize, word_ix: usize },
    Embed { node_ix: usize },
    Remove { node_ix: usize },
}

fn scripts() -> impl Strategy<Value = (usize, usize, Vec<Intent>)> {
    let intent = prop_oneof![
        (any::<usize>(), any::<usize>()).prop_map(|(f, t)| Intent::Edge { from_ix: f, to_ix: t }),
        any::<usize>().prop_map(|i| Intent::Close { edge_ix: i }),
        (any::<usize>(), any::<i64>()).prop_map(|(i, v)| Intent::SetProp { node_ix: i, val: v }),
        (any::<usize>(), any::<usize>()).prop_map(|(i, w)| Intent::SetText { node_ix: i, word_ix: w }),
        any::<usize>().prop_map(|i| Intent::Embed { node_ix: i }),
        any::<usize>().prop_map(|i| Intent::Remove { node_ix: i }),
    ];
    (3usize..10, 0usize..4, proptest::collection::vec(intent, 0..20))
}

/// Lowers the abstract script into concrete `submit_at` calls against `db`.
///
/// - Batch 0 (t=0): creates `n_scoped` nodes in one scope plus `n_shared`
///   nodes in `Scope::Shared`.
/// - Each subsequent intent becomes at most one `submit_at` at
///   `t = 1 + intent index`.
/// - `Edge`: endpoints chosen via `ix % nodes.len()`. Skipped (nothing
///   submitted) if it would be a self-loop or would violate the cross-scope
///   rule (mismatched scopes with neither endpoint `Shared`) — those would
///   be rejected anyway, but we skip up front rather than relying on
///   tolerated rejection, per the brief.
/// - `Close`: edge chosen via `ix % edges.len()`, skipped if there are no
///   edges yet. If the chosen edge happens to already be closed (or was
///   removed via a cascading `RemoveNode`), we still submit — the resulting
///   `Rejected` is tolerated and ignored, since a rejected batch appends
///   nothing to the log and so cannot affect replay determinism. This is a
///   deliberate choice (over skipping) so the script generator continues to
///   exercise the "close an already-closed/missing edge" rejection path
///   inside `apply_batch` itself, not just at replay.
/// - `SetProp`: sets `props = {"v": Some(Int(val))}` on a live node chosen
///   via `ix % nodes.len()`.
/// - `Remove`: removes a live node chosen via `ix % nodes.len()`, skipped if
///   it would empty the local node list (so every later `ix % nodes.len()`
///   stays well-defined, and `RemoveNode` never targets an already-missing
///   node).
fn run_script(db: &Db, n_scoped: usize, n_shared: usize, intents: &[Intent]) -> ScopeId {
    let scope_id = ScopeId::new();
    let scope = Scope::Id(scope_id);

    let mut nodes: Vec<(NodeId, Scope)> = Vec::new();
    let mut create_ops = Vec::new();
    for _ in 0..n_scoped {
        let id = NodeId::new();
        nodes.push((id, scope));
        create_ops.push(Op::CreateNode {
            id,
            scope,
            // "M" (not "N"): must match the `open_with` spec's declared
            // equality/text index label so `SetProp`/`SetText` actually land
            // in the indexes under test.
            label: "M".into(),
            props: Default::default(),
        });
    }
    for _ in 0..n_shared {
        let id = NodeId::new();
        nodes.push((id, Scope::Shared));
        create_ops.push(Op::CreateNode {
            id,
            scope: Scope::Shared,
            label: "M".into(),
            props: Default::default(),
        });
    }
    db.submit_at(create_ops, 0).unwrap();

    let mut edges: Vec<EdgeId> = Vec::new();

    for (i, intent) in intents.iter().enumerate() {
        let t = 1 + i as i64;
        match *intent {
            Intent::Edge { from_ix, to_ix } => {
                if nodes.is_empty() {
                    continue;
                }
                let (from_id, from_scope) = nodes[from_ix % nodes.len()];
                let (to_id, to_scope) = nodes[to_ix % nodes.len()];
                if from_id == to_id {
                    continue; // self-loop
                }
                let cross_scope_violation = from_scope != to_scope
                    && from_scope != Scope::Shared
                    && to_scope != Scope::Shared;
                if cross_scope_violation {
                    continue;
                }
                let id = EdgeId::new();
                db.submit_at(
                    vec![Op::CreateEdge {
                        id,
                        scope: from_scope,
                        ty: "REL".into(),
                        from: from_id,
                        to: to_id,
                        props: Default::default(),
                        valid_from: None,
                    }],
                    t,
                )
                .unwrap();
                edges.push(id);
            }
            Intent::Close { edge_ix } => {
                if edges.is_empty() {
                    continue;
                }
                let id = edges[edge_ix % edges.len()];
                // Tolerated: already-closed (or cascaded-away) edges yield
                // `Rejected`, which appends nothing — harmless for replay.
                let _ = db.submit_at(vec![Op::CloseEdge { id, valid_to: None }], t);
            }
            Intent::SetProp { node_ix, val } => {
                if nodes.is_empty() {
                    continue;
                }
                let (id, _) = nodes[node_ix % nodes.len()];
                let mut props: BTreeMap<String, Option<PropValue>> = BTreeMap::new();
                props.insert("v".to_string(), Some(PropValue::Int(val)));
                db.submit_at(vec![Op::SetNodeProps { id, props }], t).unwrap();
            }
            Intent::SetText { node_ix, word_ix } => {
                if nodes.is_empty() {
                    continue;
                }
                let (id, _) = nodes[node_ix % nodes.len()];
                let mut props: BTreeMap<String, Option<PropValue>> = BTreeMap::new();
                props.insert(
                    "text".to_string(),
                    Some(PropValue::Str(WORDS[word_ix % WORDS.len()].into())),
                );
                db.submit_at(vec![Op::SetNodeProps { id, props }], t).unwrap();
            }
            Intent::Embed { node_ix } => {
                if nodes.is_empty() {
                    continue;
                }
                let (id, _) = nodes[node_ix % nodes.len()];
                // Deterministic payload derived from the index so replay
                // reproduces the same embedding; exercises the `SetEmbedding`
                // arm of both `apply_op` and `Snapshot::apply` under replay.
                db.submit_at(
                    vec![Op::SetEmbedding {
                        id,
                        model: "m".into(),
                        vector: vec![node_ix as f32],
                    }],
                    t,
                )
                .unwrap();
            }
            Intent::Remove { node_ix } => {
                if nodes.len() <= 1 {
                    continue; // would empty the live node list
                }
                let ix = node_ix % nodes.len();
                let (id, _) = nodes[ix];
                db.submit_at(vec![Op::RemoveNode { id }], t).unwrap();
                nodes.remove(ix);
            }
        }
    }
    scope_id
}

/// Projects a `Snapshot`'s `nodes` map into a `BTreeMap` for order-independent
/// comparison (`im::HashMap` iteration order isn't guaranteed to match
/// between an incrementally-`apply`'d snapshot and a from-scratch
/// `Snapshot::from_storage` rebuild, even when the contents are identical).
fn nodes_map(snap: &Snapshot) -> BTreeMap<NodeId, NodeRecord> {
    snap.debug_nodes().iter().map(|(k, v)| (*k, v.clone())).collect()
}

/// Same idea for the full-record `edges` map.
fn edges_map(snap: &Snapshot) -> BTreeMap<EdgeId, EdgeRecord> {
    snap.debug_edges().iter().map(|(k, v)| (*k, v.clone())).collect()
}

/// Same idea for `out`/`inn` adjacency: per-key entry order in the `im`
/// vectors also isn't guaranteed to match, so each key's entries are sorted
/// by edge id before comparison (same technique as the `graph` module's own
/// `incremental_snapshot_equals_rebuild` test).
fn adj_map(m: &im::HashMap<NodeId, im::Vector<AdjEntry>>) -> BTreeMap<NodeId, Vec<AdjEntry>> {
    m.iter()
        .map(|(k, v)| {
            let mut entries: Vec<AdjEntry> = v.iter().cloned().collect();
            entries.sort_by_key(|e| e.edge);
            (*k, entries)
        })
        .collect()
}

/// The `IndexSpec` under which the proptest `Db` is opened: equality-indexes
/// `("M", "v")` (fed by `Intent::SetProp`) and text-indexes `("M", "text")`
/// (fed by `Intent::SetText`) — the two indexed recall paths this test guards
/// (`nodes_by_prop`, `search_text`), in addition to vector search (which
/// needs no spec declaration).
fn spec() -> IndexSpec {
    IndexSpec {
        equality: vec![PropIndex { label: "M".into(), prop: "v".into() }],
        text: vec![PropIndex { label: "M".into(), prop: "text".into() }],
    }
}

/// Equality-index and vector-search parity, checked against whatever the
/// current live state is (called both before and after
/// `rebuild_state_from_ops` — the caller supplies the same `db`/`scopes`
/// each time, so a rebuild that silently drops `prop_index` entries or
/// vector slab rows fails the *second* call even though the *first* passed).
///
/// Falsifiable: comment out `graph.rs`'s `prop_index` maintenance during
/// `Snapshot::from_storage`, or `vector.rs`'s `VectorIndex::from_snapshot`
/// seeding, and the post-rebuild call here goes from "finds it" to "doesn't"
/// while the pre-rebuild call still passes — exactly the drop this guards.
fn assert_equality_and_vector_parity(db: &Db, scopes: &ScopeSet) {
    let dump = db.debug_dump_nodes();

    // --- Equality-index parity for the declared ("M", "v") key. ---
    let mut present_v: HashSet<i64> = HashSet::new();
    for n in &dump {
        if let Some(PropValue::Int(v)) = n.props.get("v") {
            present_v.insert(*v);
            let hits = db.nodes_by_prop(scopes, "M", "v", &PropValue::Int(*v)).unwrap();
            assert!(
                hits.iter().any(|h| h.id == n.id),
                "nodes_by_prop(\"M\",\"v\",{v}) must find node {:?}",
                n.id
            );
        }
    }
    // A value no live node carries must yield nothing. `present_v` is small
    // (bounded by the script length), so a short linear probe from 0 always
    // terminates fast in practice.
    let mut absent = 0i64;
    while present_v.contains(&absent) {
        absent = absent.wrapping_add(1);
    }
    let empty_hits = db.nodes_by_prop(scopes, "M", "v", &PropValue::Int(absent)).unwrap();
    assert!(empty_hits.is_empty(), "nodes_by_prop must find nothing for unused value {absent}");

    // --- Vector parity: every embedded node's own vector must retrieve it. ---
    // k = live node count is an upper bound on any tie group (see below), so
    // asking for the whole node count guarantees the self-match can't be
    // pushed out of the top-k by ties.
    let k = dump.len().max(1);
    for n in &dump {
        if let Some((model, vector)) = &n.embedding {
            // Skip exactly-zero vectors: `vector.rs::cosine` returns `None`
            // for a zero-norm operand by design (cosine similarity is
            // undefined there), so a zero vector never scores — even against
            // itself. That is correct `search_vector` behavior, not a
            // rebuild bug, so asserting self-retrieval here would be a false
            // failure unrelated to what this test is guarding.
            if vector.iter().all(|x| *x == 0.0) {
                continue;
            }
            let hits = db
                .search_vector(&VectorQuery {
                    scopes: scopes.clone(),
                    model: model.clone(),
                    vector: vector.clone(),
                    k,
                    candidates: None,
                })
                .unwrap();
            assert!(
                hits.iter().any(|(rec, _)| rec.id == n.id),
                "search_vector must find node {:?} via its own embedding under model {model:?}",
                n.id
            );
        }
    }
}

/// Sorted result-id set for `search_text(scopes, word, k)` — used to compare
/// FTS parity before vs. after rebuild (postings must reindex to the exact
/// same set, not merely a non-empty one).
fn fts_hit_ids(db: &Db, scopes: &ScopeSet, word: &str, k: usize) -> Vec<NodeId> {
    let mut ids: Vec<NodeId> =
        db.search_text(scopes, word, k).unwrap().into_iter().map(|(n, _)| n.id).collect();
    ids.sort();
    ids
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn state_from_replay_equals_state_from_execution(script in scripts()) {
        let (n_scoped, n_shared, intents) = script;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("t.redb"), spec()).unwrap();
        let scope_id = run_script(&db, n_scoped, n_shared, &intents);
        // Covers both script scopes: the one generated scope plus Shared —
        // every read path under test (`nodes_by_prop`, `search_text`,
        // `search_vector`) is scoped, so this must see everything the script
        // created.
        let scopes = ScopeSet::of(&[scope_id]).with_shared();

        let live_nodes = db.debug_dump_nodes();
        let live_edges = db.debug_dump_edges();
        // Captured through the *reader* path (`Db::debug_snapshot`, the
        // arc-swapped in-memory snapshot readers actually see) — not
        // storage directly — so this guards the `Db::rebuild_state_from_ops`
        // snapshot swap specifically, not just `Storage`'s rebuild.
        let snap_before = db.debug_snapshot();

        // Recall-layer parity, BEFORE rebuild. `text_k` is computed from the
        // pre-rebuild live count (rebuild never changes the live set — that's
        // exactly what `live_nodes == db.debug_dump_nodes()` below asserts —
        // so the same bound stays valid for the post-rebuild calls too).
        let text_k = live_nodes.len().max(1);
        assert_equality_and_vector_parity(&db, &scopes);
        let fts_before: Vec<Vec<NodeId>> =
            WORDS.iter().map(|w| fts_hit_ids(&db, &scopes, w, text_k)).collect();

        db.rebuild_state_from_ops().unwrap();

        prop_assert_eq!(live_nodes, db.debug_dump_nodes());
        prop_assert_eq!(live_edges, db.debug_dump_edges());

        let snap_after = db.debug_snapshot();
        // `Db::rebuild_state_from_ops` must actually swap in a *new*
        // `Arc<Snapshot>` — not just leave the old one in place. Checking
        // pointer identity (rather than only content) is what catches a
        // dropped/forgotten `snap.store(...)` call: since a correct rebuild
        // reproduces identical content, a content-only comparison would
        // still pass even if the swap never happened (the stale Arc already
        // held the same values). This is the reader-facing guard the
        // reviewer asked for.
        prop_assert!(
            !Arc::ptr_eq(&snap_before, &snap_after),
            "rebuild_state_from_ops must store a fresh Snapshot Arc, not leave the old one in place"
        );
        prop_assert_eq!(nodes_map(&snap_before), nodes_map(&snap_after));
        prop_assert_eq!(edges_map(&snap_before), edges_map(&snap_after));
        prop_assert_eq!(adj_map(snap_before.debug_out()), adj_map(snap_after.debug_out()));
        prop_assert_eq!(adj_map(snap_before.debug_inn()), adj_map(snap_after.debug_inn()));

        // Recall-layer parity, AFTER rebuild. Equality/vector re-assert the
        // same "finds it" property against the rebuilt state; FTS asserts the
        // *exact same* result-id set as before rebuild — this is the one
        // group compared by equality rather than just re-affirmed, since the
        // brief calls for identical sets, not merely non-empty ones.
        assert_equality_and_vector_parity(&db, &scopes);
        let fts_after: Vec<Vec<NodeId>> =
            WORDS.iter().map(|w| fts_hit_ids(&db, &scopes, w, text_k)).collect();
        prop_assert_eq!(fts_before, fts_after);
    }
}
