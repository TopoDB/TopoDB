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
pub fn link_superseding(
    db: &Db,
    scope: Scope,
    from: NodeId,
    to: NodeId,
    ty: &str,
    now_ms: i64,
) -> Result<EdgeId, SghError> {
    let scopes = match scope {
        Scope::Id(id) => ScopeSet::of(&[id]),
        Scope::Shared => ScopeSet::of(&[]).with_shared(),
    };

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
        let open: Vec<_> = sg.edges.into_iter().filter(|e| e.from == from).collect();

        // Already correct: nothing to do.
        if let Some(existing) = open.iter().find(|e| e.to == to) {
            return Ok(existing.id);
        }

        let mut ops: Vec<Op> = open
            .iter()
            .map(|e| Op::CloseEdge { id: e.id, valid_to: Some(now_ms) })
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
            // A competing writer closed an edge between our read and our write.
            // Re-read and try again.
            Err(topodb::TopoError::Rejected(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }

    Err(SghError::Contended { attempts: MAX_ATTEMPTS })
}
