//! Deterministic synthetic agent-memory workload for storage benchmarks.

use crate::ids::{EdgeId, NodeId, Scope, ScopeId};
use crate::op::Op;
use crate::props::PropValue;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct WorkloadSpec {
    pub memories: usize,
    pub seed: u64,
    pub embed_dim: usize,
    pub embed_pct: u8,
}
impl Default for WorkloadSpec {
    fn default() -> Self {
        Self {
            memories: 10_000,
            seed: 0xC0FFEE,
            embed_dim: 768,
            embed_pct: 20,
        }
    }
}
struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
const WORDS: [&str; 16] = [
    "agent", "memory", "graph", "scope", "recall", "vector", "search", "index", "temporal", "edge",
    "node", "label", "batch", "snapshot", "project", "decision",
];
fn memory_id(i: usize) -> NodeId {
    NodeId::from_u128(0x0100_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}
fn entity_id(i: usize) -> NodeId {
    NodeId::from_u128(0x0200_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}
fn edge_id(i: usize) -> EdgeId {
    EdgeId::from_u128(0x0300_0000_0000_0000_0000_0000_0000_0000 | i as u128)
}
fn sentence(r: &mut SplitMix64) -> String {
    (0..50 + r.below(451))
        .map(|_| WORDS[r.below(WORDS.len())])
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn batches(spec: &WorkloadSpec) -> Vec<Vec<Op>> {
    let mut r = SplitMix64(spec.seed);
    let scope = Scope::Id(ScopeId::from_u128(1));
    let entities = (spec.memories / 5).max(1);
    let mut ops = Vec::new();
    for i in 0..entities {
        let mut p = BTreeMap::new();
        p.insert("name".into(), PropValue::Str(format!("entity-{i}")));
        ops.push(Op::CreateNode {
            id: entity_id(i),
            scope,
            label: "Entity".into(),
            props: p,
        });
    }
    let mut e = 0;
    for i in 0..spec.memories {
        let mut p = BTreeMap::new();
        p.insert("content".into(), PropValue::Str(sentence(&mut r)));
        p.insert(
            "created_at".into(),
            PropValue::Int(1_700_000_000_000 + i as i64),
        );
        ops.push(Op::CreateNode {
            id: memory_id(i),
            scope,
            label: "Memory".into(),
            props: p,
        });
        let at = Some(1_700_000_000_000 + i as i64);
        ops.push(Op::CreateEdge {
            id: edge_id(e),
            scope,
            ty: "ABOUT".into(),
            from: memory_id(i),
            to: entity_id(r.below(entities)),
            props: BTreeMap::new(),
            valid_from: at,
        });
        e += 1;
        if r.below(2) == 0 {
            ops.push(Op::CreateEdge {
                id: edge_id(e),
                scope,
                ty: "MENTIONS".into(),
                from: memory_id(i),
                to: entity_id(r.below(entities)),
                props: BTreeMap::new(),
                valid_from: at,
            });
            e += 1;
        }
        if i > 0 {
            ops.push(Op::CreateEdge {
                id: edge_id(e),
                scope,
                ty: "FOLLOWS".into(),
                from: memory_id(i),
                to: memory_id(i - 1),
                props: BTreeMap::new(),
                valid_from: at,
            });
            e += 1;
        }
        if i < spec.memories * spec.embed_pct as usize / 100 {
            let v = (0..spec.embed_dim)
                .map(|_| (r.next() as f32 / u64::MAX as f32) * 2. - 1.)
                .collect();
            ops.push(Op::SetEmbedding {
                id: memory_id(i),
                model: "bench-768".into(),
                vector: v,
            });
        }
    }
    ops.chunks(200).map(|x| x.to_vec()).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workload_is_deterministic_and_shaped() {
        let s = WorkloadSpec {
            memories: 100,
            ..Default::default()
        };
        let a = batches(&s);
        assert_eq!(a, batches(&s));
        let o: Vec<_> = a.iter().flatten().collect();
        assert_eq!(
            o.iter()
                .filter(|x| matches!(x, Op::CreateNode { .. }))
                .count(),
            120
        );
        assert_eq!(
            o.iter()
                .filter(|x| matches!(x, Op::SetEmbedding { .. }))
                .count(),
            20
        );
        assert!(a.iter().all(|x| !x.is_empty() && x.len() <= 200));
    }
}
