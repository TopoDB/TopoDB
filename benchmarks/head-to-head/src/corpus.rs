//! One logical dataset, two on-disk shapes.
//!
//! Neither engine may receive a corpus tuned to its model, so both shapes are
//! derived from the same `LogicalNode`/`LogicalEdge` values. The translation
//! choice — how many EAV facts equal one property-graph node — is reported
//! rather than buried, because that ratio is exactly where a cross-engine
//! benchmark can quietly mislead.
//!
//! **Write-once policy**: minigraf has no automatic last-write-wins — a
//! second `(transact ...)` for the same `[entity attribute]` leaves both
//! values simultaneously "current" forever unless the old one is explicitly
//! retracted (see `docs/superpowers/notes/2026-07-18-minigraf-api-findings.md`
//! §5). TopoDB's `SetNodeProps`, by contrast, overwrites natively. To keep
//! the two engines comparable without introducing retract-pairing machinery
//! that only one side needs, this corpus is generated **write-once**: every
//! entity/attribute is asserted exactly once and never updated. `Corpus`
//! deliberately exposes no update or mutation API — this is a design
//! decision, not an oversight.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Property names used by every generated node. Fixed so the two shapes and
/// successive runs stay comparable.
pub const PROP_NAMES: [&str; 5] = ["name", "kind", "note", "rank", "active"];

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalNode {
    pub id: usize,
    pub name: String,
    pub kind: String,
    pub note: String,
    pub rank: i64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalEdge {
    pub from: usize,
    pub to: usize,
    pub ty: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    pub nodes: usize,
    pub props: usize,
    pub edges: usize,
    pub facts: usize,
}

/// A single logical dataset, generated once and never mutated. See the
/// module doc comment for why this corpus is write-once.
#[derive(Debug, Clone, PartialEq)]
pub struct Corpus {
    pub seed: u64,
    pub nodes: Vec<LogicalNode>,
    pub edges: Vec<LogicalEdge>,
}

const EDGE_TYPES: [&str; 3] = ["RELATES_TO", "MENTIONS", "DERIVED_FROM"];

impl Corpus {
    /// Generate `nodes` nodes and roughly `3 * nodes` edges from `seed`.
    ///
    /// Edges point backwards in the node list so the graph is acyclic and
    /// every reference resolves by construction. Each node past the first
    /// gets 1-3 backward edges, which keeps the graph connected and
    /// traversable at depth 4 without collapsing into a dense blob.
    pub fn generate(seed: u64, nodes: usize) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);

        let generated: Vec<LogicalNode> = (0..nodes)
            .map(|id| LogicalNode {
                id,
                name: format!("node-{id}"),
                kind: EDGE_TYPES[rng.gen_range(0..EDGE_TYPES.len())].to_lowercase(),
                note: format!("note body for node {id} with some searchable words"),
                rank: rng.gen_range(0..1000),
                active: rng.gen_bool(0.5),
            })
            .collect();

        let mut edges = Vec::new();
        for id in 1..nodes {
            // 1..=3 backward edges per node keeps the graph traversable at
            // depth 4 without exploding into a dense blob.
            let count = rng.gen_range(1..=3);

            // `from` is fixed to `id` for every edge drawn in this loop
            // iteration, so deduplicating (to, ty) pairs within this node's
            // own edges is sufficient to guarantee global (from, to, ty)
            // uniqueness across the whole corpus: no other iteration can ever
            // produce an edge with this `from`.
            //
            // Membership is checked against a plain Vec built in draw order
            // (not a HashSet) so no hash-iteration order can reach the
            // output, keeping generation byte-identical across processes for
            // a given seed. Re-rolls just keep consuming the same seeded RNG
            // sequence, so they stay deterministic too.
            //
            // The attempt cap guards against small candidate spaces (node 1
            // has only one possible target, so at most
            // `EDGE_TYPES.len()` == 3 distinct edges exist for it at all):
            // once exhausted, the node simply ends up with fewer than
            // `count` edges rather than looping forever.
            let mut node_edges: Vec<LogicalEdge> = Vec::with_capacity(count);
            let max_attempts = 32;
            let mut attempts = 0;
            while node_edges.len() < count && attempts < max_attempts {
                attempts += 1;
                let target = rng.gen_range(0..id);
                let ty = EDGE_TYPES[rng.gen_range(0..EDGE_TYPES.len())].to_string();
                if node_edges.iter().any(|e| e.to == target && e.ty == ty) {
                    continue;
                }
                node_edges.push(LogicalEdge { from: id, to: target, ty });
            }
            edges.extend(node_edges);
        }

        Corpus {
            seed,
            nodes: generated,
            edges,
        }
    }

    /// The translation choice, reported in every benchmark run.
    ///
    /// A property-graph node with N props becomes N EAV facts; an edge becomes
    /// one fact. Any reader can therefore check whether the two engines were
    /// asked to store comparable amounts of information. `props` is derived
    /// from `PROP_NAMES.len()` so the ratio tracks the schema rather than a
    /// hardcoded constant.
    pub fn translation_ratio(&self) -> Ratio {
        let props = self.nodes.len() * PROP_NAMES.len();
        Ratio {
            nodes: self.nodes.len(),
            props,
            edges: self.edges.len(),
            facts: props + self.edges.len(),
        }
    }

    /// How many distinct nodes are reachable from `seed_idx` within `depth`
    /// hops, following edges in both directions. Used to assert the corpus is
    /// actually traversable before benchmarking traversal on it.
    pub fn reachable_within(&self, seed_idx: usize, depth: u8) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut frontier = vec![seed_idx];
        seen.insert(seed_idx);

        for _ in 0..depth {
            let mut next = Vec::new();
            for &cur in &frontier {
                for e in &self.edges {
                    let neighbour = if e.from == cur {
                        Some(e.to)
                    } else if e.to == cur {
                        Some(e.from)
                    } else {
                        None
                    };
                    if let Some(n) = neighbour {
                        if seen.insert(n) {
                            next.push(n);
                        }
                    }
                }
            }
            frontier = next;
        }

        seen.len()
    }
}
