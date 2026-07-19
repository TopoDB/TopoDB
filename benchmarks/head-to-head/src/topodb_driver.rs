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
        let sid = ScopeId::new();
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
            let id = NodeId::new();
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
        let Some(&node_id) = self.ids.get(&id) else {
            return Ok(None);
        };
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
        let Some(&node_id) = self.ids.get(&seed) else {
            return Ok(0);
        };
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
