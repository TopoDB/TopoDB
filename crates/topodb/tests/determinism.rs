use proptest::prelude::*;
use std::collections::BTreeMap;
use topodb::*;

/// Generate a small random-but-valid op sequence: create nodes, then a mix
/// of edges/prop-sets/closes/removes referencing only created ids.
/// Abstract intents, lowered to concrete Ops inside the test where real ids
/// exist. Indices are taken modulo the live collection length, so every
/// generated script is valid by construction.
#[derive(Debug, Clone)]
enum Intent {
    Edge { from_ix: usize, to_ix: usize },
    Close { edge_ix: usize },
    SetProp { node_ix: usize, val: i64 },
    Remove { node_ix: usize },
}

fn scripts() -> impl Strategy<Value = (usize, usize, Vec<Intent>)> {
    let intent = prop_oneof![
        (any::<usize>(), any::<usize>()).prop_map(|(f, t)| Intent::Edge { from_ix: f, to_ix: t }),
        any::<usize>().prop_map(|i| Intent::Close { edge_ix: i }),
        (any::<usize>(), any::<i64>()).prop_map(|(i, v)| Intent::SetProp { node_ix: i, val: v }),
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
fn run_script(db: &Db, n_scoped: usize, n_shared: usize, intents: &[Intent]) {
    let scope = Scope::Id(ScopeId::new());

    let mut nodes: Vec<(NodeId, Scope)> = Vec::new();
    let mut create_ops = Vec::new();
    for _ in 0..n_scoped {
        let id = NodeId::new();
        nodes.push((id, scope));
        create_ops.push(Op::CreateNode {
            id,
            scope,
            label: "N".into(),
            props: Default::default(),
        });
    }
    for _ in 0..n_shared {
        let id = NodeId::new();
        nodes.push((id, Scope::Shared));
        create_ops.push(Op::CreateNode {
            id,
            scope: Scope::Shared,
            label: "N".into(),
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
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn state_from_replay_equals_state_from_execution(script in scripts()) {
        let (n_scoped, n_shared, intents) = script;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.redb")).unwrap();
        run_script(&db, n_scoped, n_shared, &intents);

        let live_nodes = db.debug_dump_nodes();
        let live_edges = db.debug_dump_edges();

        db.rebuild_state_from_ops().unwrap();

        prop_assert_eq!(live_nodes, db.debug_dump_nodes());
        prop_assert_eq!(live_edges, db.debug_dump_edges());
    }
}
