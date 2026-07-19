use std::collections::HashMap;
use std::path::{Path, PathBuf};

use topodb::{
    Db, Direction, EdgeId, NodeId, Op, PropValue, Props, Scope, ScopeId, ScopeSet, TraversalQuery,
};

use crate::corpus::Corpus;
use crate::engine::{AsOfSupport, Engine, EngineError, Payload};

const LABEL: &str = "BenchNode";

pub struct TopoDbDriver {
    db: Db,
    path: PathBuf,
    scope: Scope,
    scopes: ScopeSet,
    /// logical corpus id -> engine node id
    ids: HashMap<usize, NodeId>,
}

fn err<E: std::fmt::Display>(e: E) -> EngineError {
    EngineError::Backend(e.to_string())
}

impl Engine for TopoDbDriver {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let db = Db::open(path).map_err(err)?;
        // Deterministic, not `ScopeId::new()`: a random scope id here would
        // only ever match the scope actually written to disk if this same
        // process instance also performed the insert. A genuinely fresh
        // reopen (the "cold" case in the point_query benchmark) needs to
        // land on the exact scope used at insert time, so the id is fixed
        // rather than randomly minted per `open()` call.
        let sid = ScopeId::from_u128(0xB0F0);
        Ok(TopoDbDriver {
            db,
            path: path.to_path_buf(),
            scope: Scope::Id(sid),
            scopes: ScopeSet::of(&[sid]),
            ids: HashMap::new(),
        })
    }

    fn insert_corpus(&mut self, corpus: &Corpus) -> Result<(), EngineError> {
        // One batch for nodes, one for edges. Explicit timestamps, never wall
        // clock, so the run is reproducible.
        let mut ops = Vec::with_capacity(corpus.nodes.len());
        for n in &corpus.nodes {
            // Deterministic, not `NodeId::new()`: the logical id -> engine id
            // map otherwise lives only in this process's `self.ids`, which a
            // fresh `Engine::open` (a genuinely cold reopen, e.g. in the
            // point_query benchmark) cannot reconstruct from disk alone.
            // Deriving the engine id from the logical id makes `point_lookup`
            // and `k_hop` work correctly against a freshly opened handle with
            // an empty `self.ids`, which is what a real "cold" measurement
            // requires.
            let id = NodeId::from_u128(n.id as u128);
            self.ids.insert(n.id, id);

            let mut props = Props::new();
            props.insert("name".into(), PropValue::Str(n.name.clone()));
            props.insert("kind".into(), PropValue::Str(n.kind.clone()));
            props.insert("note".into(), PropValue::Str(n.note.clone()));
            props.insert("rank".into(), PropValue::Int(n.rank));
            props.insert("active".into(), PropValue::Bool(n.active));

            ops.push(Op::CreateNode {
                id,
                scope: self.scope,
                label: LABEL.into(),
                props,
            });
        }
        self.db.submit_at(ops, 1).map_err(err)?;

        let mut ops = Vec::with_capacity(corpus.edges.len());
        for e in &corpus.edges {
            ops.push(Op::CreateEdge {
                id: EdgeId::new(),
                scope: self.scope,
                ty: e.ty.as_str().into(),
                from: self.ids[&e.from],
                to: self.ids[&e.to],
                props: Props::new(),
                valid_from: Some(2),
            });
        }
        self.db.submit_at(ops, 2).map_err(err)?;
        Ok(())
    }

    fn point_lookup(&self, id: usize) -> Result<Option<Payload>, EngineError> {
        // Deterministic id derivation (see `insert_corpus`) so this works
        // against a freshly opened handle whose `self.ids` is empty, not
        // just the instance that performed the insert.
        let node_id = NodeId::from_u128(id as u128);
        let Some(rec) = self.db.node(&self.scopes, node_id) else {
            return Ok(None);
        };

        let name = match rec.props.get("name") {
            Some(PropValue::Str(s)) => s.clone(),
            _ => return Ok(None),
        };
        let rank = match rec.props.get("rank") {
            Some(PropValue::Int(i)) => *i,
            _ => return Ok(None),
        };
        Ok(Some(Payload { name, rank }))
    }

    fn k_hop(&self, seed: usize, depth: u8) -> Result<usize, EngineError> {
        let node_id = NodeId::from_u128(seed as u128);
        let sub = self
            .db
            .traverse(&TraversalQuery {
                scopes: self.scopes.clone(),
                seeds: vec![node_id],
                max_hops: depth,
                edge_types: None,
                direction: Direction::Both,
                as_of: None,
            })
            .map_err(err)?;
        // `Subgraph::nodes` includes the seed itself (verified against
        // `crates/topodb/src/read.rs`: the traversal seeds `visited`/frontier
        // with the seed slot before walking, at hop 0, and every visited slot
        // is materialised into `nodes_out`). This matches
        // `Corpus::reachable_within`, which also seeds `seen` with the seed
        // index before walking — see `benchmarks/head-to-head/tests/drivers.rs`
        // `topodb_k_hop_matches_corpus_reachable_within_seed_counting`, which
        // asserts the two counts agree for the same seed/depth.
        Ok(sub.nodes.len())
    }

    fn on_disk_bytes(&self) -> Result<u64, EngineError> {
        Ok(std::fs::metadata(&self.path).map_err(err)?.len())
    }

    fn as_of_support() -> AsOfSupport {
        // Node props are overwrite-only (`op.rs:19-20`), and `traverse(as_of)`
        // returns historical topology with present-day payloads. Not the same
        // operation, so not claimed as one.
        AsOfSupport::NodePayloadUnsupported
    }
}
