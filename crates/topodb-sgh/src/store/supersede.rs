use topodb::{Db, Direction, EdgeId, NodeId, Op, Props, Scope, ScopeSet, TraversalQuery};

use super::SghError;

/// How many times to retry a lost supersession race before giving up.
/// `crates/topodb/tests/concurrency.rs` uses 64 for a 16-writer stress test;
/// sgh's executor has far less contention, so 16 is ample.
const MAX_ATTEMPTS: u32 = 16;

/// Create `from -[ty]-> to`, closing every other open edge of type `ty` out of
/// `from`. This is the supersession policy the engine does not provide: the
/// engine has no uniqueness constraint on `(from, ty)`, so a second
/// `CreateEdge` alone would leave both edges open.
///
/// Idempotent — if an open edge to the same target already exists, its id is
/// returned and nothing is written.
///
/// Closes and the create go in one `submit_at`, so supersession is atomic.
/// The preceding read is not, hence the retry loop.
///
/// # Invariant this relies on: `as_of: Some(now_ms)` reads the open edge set
///
/// The read below asks `traverse` for edges valid `as_of: Some(now_ms)`,
/// whose predicate is `valid_from <= t && valid_to.is_none_or(|vt| t < vt)`
/// (`crates/topodb/src/read.rs`). That is only equivalent to "read the
/// currently-open edges" — as opposed to "read the edges that happened to be
/// open at some past instant `now_ms`" — under two conditions this module
/// must preserve:
///
/// 1. Callers supply non-decreasing `now_ms` for a given `(from, ty)`. This
///    function does not check monotonicity; it trusts the caller.
/// 2. `link_superseding` never closes an edge with a `valid_to` in the
///    future (it always closes with the same `now_ms` it's writing the new
///    edge at, never a later one).
///
/// If either condition is violated by a caller or a future change to this
/// function, "open as of `now_ms`" and "open right now" diverge silently.
pub fn link_superseding(
    db: &Db,
    scope: Scope,
    from: NodeId,
    to: NodeId,
    ty: &str,
    now_ms: i64,
) -> Result<EdgeId, SghError> {
    let scopes = match scope {
        // Shared-scope edges are deliberately excluded here: an Id-scoped
        // supersession only ever creates and reads edges within that one
        // scope, so widening the read to include shared-scope edges would
        // let an out-of-scope edge participate in an in-scope supersession
        // decision. `Scope::Shared` below is the mirror case — it reads
        // (and writes) only the shared scope.
        Scope::Id(id) => ScopeSet::of(&[id]),
        Scope::Shared => ScopeSet::of(&[]).with_shared(),
    };

    // Pre-check both endpoints once, outside the retry loop. `TopoError::
    // Rejected` is engine-neutral (see below) and covers both a lost race
    // *and* a permanent bad input such as a stale/nonexistent NodeId; without
    // this check the latter would burn all `MAX_ATTEMPTS` retries before
    // surfacing as a misleading `Contended`. This does not make the
    // subsequent write race-free (a node could vanish between this check and
    // the `submit_at` below) — it only removes the common, permanent,
    // never-a-race case up front.
    for node in [from, to] {
        if db.node(&scopes, node).is_none() {
            return Err(SghError::MissingEndpoint { node });
        }
    }

    for _ in 0..MAX_ATTEMPTS {
        // `Db` has no `edges_from`; `traverse` (1 hop, `Direction::Out`, a
        // type filter, `as_of: Some(now_ms)`) is the public read primitive
        // that answers "what is open out of `from` via `ty` right now".
        let sg = db.traverse(&TraversalQuery {
            scopes: scopes.clone(),
            seeds: vec![from],
            max_hops: 1,
            edge_types: Some(vec![ty.into()]),
            direction: Direction::Out,
            as_of: Some(now_ms),
        })?;
        // No `.filter(|e| e.from == from)` here: `sg.edges` is already exactly
        // the edges traversed outward, 1 hop, from the single seed `from`
        // (`Direction::Out`), so every edge in it already has `.from ==
        // from` by construction of the traversal itself.
        let open: Vec<_> = sg.edges;

        // Already correct: nothing to do.
        if let Some(existing) = open.iter().find(|e| e.to == to) {
            return Ok(existing.id);
        }

        let mut ops: Vec<Op> = open
            .iter()
            .map(|e| Op::CloseEdge {
                id: e.id,
                valid_to: Some(now_ms),
            })
            .collect();

        let new_id = EdgeId::new();
        ops.push(Op::CreateEdge {
            id: new_id,
            scope,
            ty: ty.into(),
            from,
            to,
            props: Props::new(),
            valid_from: Some(now_ms),
        });

        match db.submit_at(ops, now_ms) {
            Ok(_) => return Ok(new_id),
            // `TopoError::Rejected` is deliberately neutral (see
            // `crates/topodb/src/error.rs`): the engine returns it both for
            // a lost race (a competing writer closed one of the edges we're
            // trying to close, or created a conflicting one, between our
            // read and this write) and for permanent invalid input (e.g.
            // `CreateEdge`/`CloseEdge` referencing a node or edge that no
            // longer exists). The endpoint pre-check above rules out the
            // most common permanent case (a missing `from`/`to` node) before
            // we ever get here, but it can't close every gap — an edge could
            // still vanish between our read and this `submit_at`, or a node
            // could be deleted concurrently. So this arm still can't tell
            // "lost race, try again" apart from "some other permanent
            // rejection" with certainty; bounding attempts at MAX_ATTEMPTS is
            // what keeps that residual ambiguity from looping forever.
            Err(topodb::TopoError::Rejected(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    Err(SghError::Contended {
        attempts: MAX_ATTEMPTS,
    })
}
