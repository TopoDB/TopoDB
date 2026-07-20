//! Write-path validation that is deliberately NOT in `storage.rs`.
//!
//! `storage::apply_op` is shared by live writes and by op-log replay
//! (`rebuild_state_from_ops`). A rule enforced there would retroactively
//! condemn already-committed data: a database whose log holds an edge that was
//! legal when it was written would become un-rebuildable, its only repair path
//! refusing a log that is old rather than corrupt. So this runs on live submit
//! only, from the applier thread in `db.rs`, above `Storage`.
//!
//! See `specs/2026-07-11-d3-edge-scope-validation-design.md` §3.

use crate::error::TopoError;
use crate::ids::{NodeId, Scope};
use crate::op::Op;
use crate::state::NodeRecord;
use std::collections::HashMap;

/// Is `edge` a legal scope for an edge between endpoints scoped `from` and `to`?
///
/// The rule: **if either endpoint is project-scoped `Scope::Id(a)`, the edge must
/// be scoped `a` or `Scope::Shared`. If both endpoints are `Shared`, any edge
/// scope is allowed.**
///
/// The permissive branch is not laziness. An edge scoped to project P between two
/// `Shared` nodes is the "project-private relationship between shared entities"
/// pattern — P's reader (`{P, Shared}`) traverses it, another project's reader
/// (`{Other, Shared}`) does not.
///
/// The caller need not check that the endpoints themselves are a legal pair;
/// `apply_op` already rejects two *different* project scopes without a `Shared`
/// endpoint, so at most one distinct project scope reaches here.
pub(crate) fn edge_scope_is_valid(edge: Scope, from: Scope, to: Scope) -> bool {
    match (from, to) {
        (Scope::Shared, Scope::Shared) => true,
        _ => {
            let endpoint = match from {
                Scope::Id(_) => from,
                Scope::Shared => to,
            };
            edge == endpoint || edge == Scope::Shared
        }
    }
}

/// Rejects any `CreateEdge` in `ops` whose scope is illegal for its endpoints.
/// Runs on the applier thread before `apply_batch`, so a violation leaves storage
/// untouched. An endpoint's scope comes from a same-batch `CreateNode` if present,
/// otherwise from `pre` (a storage read of pre-batch node state taken by the
/// applier before `apply_batch`, keyed by every id this batch might
/// reference); missing endpoints are left for `apply_batch` to reject.
pub(crate) fn prevalidate_edge_scopes(
    pre: &HashMap<NodeId, NodeRecord>,
    ops: &[Op],
) -> Result<(), TopoError> {
    let mut created_scope: HashMap<NodeId, Scope> = HashMap::new();
    for op in ops {
        match op {
            Op::CreateNode { id, scope, .. } => {
                created_scope.insert(*id, *scope);
            }
            Op::CreateEdge {
                id,
                scope,
                from,
                to,
                ..
            } => {
                let scope_of = |node: &NodeId| -> Option<Scope> {
                    created_scope
                        .get(node)
                        .copied()
                        .or_else(|| pre.get(node).map(|record| record.scope))
                };
                let (Some(from_scope), Some(to_scope)) = (scope_of(from), scope_of(to)) else {
                    continue;
                };
                if !edge_scope_is_valid(*scope, from_scope, to_scope) {
                    let endpoint = match from_scope {
                        Scope::Id(_) => from_scope,
                        Scope::Shared => to_scope,
                    };
                    return Err(TopoError::Rejected(format!(
                        "CreateEdge {id:?}: edge scope {} is unrelated to its endpoints \
                         (from: {}, to: {}). An edge touching a node in scope {} must be \
                         scoped {} or shared, or it is invisible to readers of {} and \
                         visible to readers of {}.",
                        label(*scope),
                        label(from_scope),
                        label(to_scope),
                        label(endpoint),
                        label(endpoint),
                        label(endpoint),
                        label(*scope),
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Rejects any `CreateNode` in `ops` whose id already resolves to a node —
/// either in `pre` (a storage read of pre-batch node state taken by the
/// applier before `apply_batch`, keyed by every id this batch might
/// reference, or the same-group overlay from an earlier batch in this
/// group — see `db.rs::apply_group`) or earlier in the SAME `ops` slice (two
/// `CreateNode`s for the same id in one batch).
///
/// Ids are mint-once: every in-tree writer (MCP, the batch DSL) always mints
/// a fresh ULID, so a `CreateNode` for an id that already exists is not a
/// legitimate "upsert" request — it was, until this check, silently accepted
/// as one, and that upsert path left `LABEL_INDEX` (and, before this branch,
/// FTS/prop-index state) permanently stale: the new `(label, scope, id)` key
/// was inserted but the old one was never removed, so label reads could
/// return wrong-label hits and leak a node across a scope boundary. Runs on
/// live submit only, same as `prevalidate_edge_scopes` above and for the
/// same reason: this must not run inside `apply_op`, which is shared with
/// op-log replay, and a rule enforced there would retroactively condemn an
/// already-committed historic log. Replay stays tolerant; `storage.rs`'s
/// `load_nodes_by_label`/`load_nodes_by_label_newest` re-filter against the
/// fetched record as defense for whatever a historic log still contains.
pub(crate) fn prevalidate_create_node_ids(
    pre: &HashMap<NodeId, NodeRecord>,
    ops: &[Op],
) -> Result<(), TopoError> {
    let mut seen_in_batch: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    for op in ops {
        if let Op::CreateNode { id, .. } = op {
            if pre.contains_key(id) || !seen_in_batch.insert(*id) {
                return Err(TopoError::Rejected(format!(
                    "CreateNode {id:?}: id already exists — ids are mint-once; use \
                     SetNodeProps to update an existing node instead of re-creating it"
                )));
            }
        }
    }
    Ok(())
}

fn label(scope: Scope) -> String {
    match scope {
        Scope::Shared => "shared".to_string(),
        Scope::Id(id) => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ScopeId;

    #[test]
    fn shared_endpoints_admit_any_edge_scope() {
        let p = Scope::Id(ScopeId::new());
        assert!(edge_scope_is_valid(p, Scope::Shared, Scope::Shared));
        assert!(edge_scope_is_valid(
            Scope::Shared,
            Scope::Shared,
            Scope::Shared
        ));
    }

    #[test]
    fn project_scoped_endpoint_admits_only_its_own_scope_or_shared() {
        let a = Scope::Id(ScopeId::new());
        assert!(edge_scope_is_valid(a, a, a));
        assert!(edge_scope_is_valid(Scope::Shared, a, a));
        assert!(edge_scope_is_valid(a, a, Scope::Shared));
        assert!(edge_scope_is_valid(a, Scope::Shared, a));
        assert!(edge_scope_is_valid(Scope::Shared, a, Scope::Shared));
        assert!(edge_scope_is_valid(Scope::Shared, Scope::Shared, a));
    }

    #[test]
    fn unrelated_project_scope_is_invalid_when_an_endpoint_is_project_scoped() {
        let a = Scope::Id(ScopeId::new());
        let p = Scope::Id(ScopeId::new());
        assert!(!edge_scope_is_valid(p, a, a));
        assert!(!edge_scope_is_valid(p, a, Scope::Shared));
        assert!(!edge_scope_is_valid(p, Scope::Shared, a));
    }

    fn node_record(id: NodeId, scope: Scope) -> NodeRecord {
        NodeRecord {
            id,
            scope,
            label: Default::default(),
            props: Default::default(),
            embedding: None,
        }
    }

    fn create_op(id: NodeId) -> Op {
        Op::CreateNode {
            id,
            scope: Scope::Shared,
            label: "X".into(),
            props: Default::default(),
        }
    }

    /// Direct unit coverage of `prevalidate_create_node_ids` — the function
    /// both `apply_one_job` and `apply_group` (`db.rs`) call. `apply_group`
    /// feeds it `effective_pre`, which already has the same-group overlay
    /// folded in (`Some(scope)` for an earlier batch's `CreateNode`), so
    /// exercising "id present in `pre`" here covers that path's contract
    /// too, without needing to force an actual optimistic group commit.
    #[test]
    fn fresh_id_is_accepted() {
        let pre = HashMap::new();
        let ops = vec![create_op(NodeId::new())];
        assert!(prevalidate_create_node_ids(&pre, &ops).is_ok());
    }

    #[test]
    fn id_already_present_in_pre_state_is_rejected() {
        let id = NodeId::new();
        let mut pre = HashMap::new();
        pre.insert(id, node_record(id, Scope::Shared));
        let ops = vec![create_op(id)];
        assert!(matches!(
            prevalidate_create_node_ids(&pre, &ops),
            Err(TopoError::Rejected(_))
        ));
    }

    #[test]
    fn duplicate_id_within_the_same_ops_slice_is_rejected() {
        let pre = HashMap::new();
        let id = NodeId::new();
        let ops = vec![create_op(id), create_op(id)];
        assert!(matches!(
            prevalidate_create_node_ids(&pre, &ops),
            Err(TopoError::Rejected(_))
        ));
    }

    #[test]
    fn distinct_ids_and_non_create_ops_are_unaffected() {
        let pre = HashMap::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let ops = vec![
            create_op(a),
            create_op(b),
            Op::SetNodeProps {
                id: a,
                props: Default::default(),
            },
        ];
        assert!(prevalidate_create_node_ids(&pre, &ops).is_ok());
    }
}
