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
use crate::graph::Snapshot;
use crate::ids::{NodeId, Scope};
use crate::op::Op;
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
/// otherwise from `cur`; missing endpoints are left for `apply_batch` to reject.
pub(crate) fn prevalidate_edge_scopes(cur: &Snapshot, ops: &[Op]) -> Result<(), TopoError> {
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
                        .or_else(|| cur.nodes.get(node).map(|record| record.scope))
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
}
