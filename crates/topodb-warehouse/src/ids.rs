//! Deterministic ULIDs for derived nodes/edges: top 48 bits = a real
//! timestamp (recency math stays sane), low 80 bits = blake3 of the parts.
use topodb::{EdgeId, NodeId};

pub fn derived_u128(ts_ms: i64, parts: &[&[u8]]) -> u128 {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(p);
        h.update(&[0x1f]);
    }
    let d = h.finalize();
    let mut low = [0u8; 16];
    low[6..16].copy_from_slice(&d.as_bytes()[0..10]);
    let rand = u128::from_be_bytes(low); // 80 bits
    ulid::Ulid::from_parts(ts_ms.max(0) as u64, rand).0
}
pub fn artifact_node_id(scope: &str, hash: &str, first_seen_ms: i64) -> NodeId {
    NodeId::from_u128(derived_u128(
        first_seen_ms,
        &[b"artifact", scope.as_bytes(), hash.as_bytes()],
    ))
}
pub fn chunk_node_id(artifact: NodeId, idx: u32, derive_version: u32, ts_ms: i64) -> NodeId {
    NodeId::from_u128(derived_u128(
        ts_ms,
        &[
            b"chunk",
            &artifact.as_u128().to_be_bytes(),
            &idx.to_be_bytes(),
            &derive_version.to_be_bytes(),
        ],
    ))
}
pub fn has_chunk_edge_id(artifact: NodeId, chunk: NodeId, ts_ms: i64) -> EdgeId {
    EdgeId::from_u128(derived_u128(
        ts_ms,
        &[
            b"has_chunk",
            &artifact.as_u128().to_be_bytes(),
            &chunk.as_u128().to_be_bytes(),
        ],
    ))
}
pub fn evidence_edge_id(memory: NodeId, artifact: NodeId, rule: &str, ts_ms: i64) -> EdgeId {
    EdgeId::from_u128(derived_u128(
        ts_ms,
        &[
            b"evidence",
            &memory.as_u128().to_be_bytes(),
            &artifact.as_u128().to_be_bytes(),
            rule.as_bytes(),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_deterministic_and_carry_timestamp() {
        let a = artifact_node_id("shared", "b3:abc", 1_700_000_000_000);
        let b = artifact_node_id("shared", "b3:abc", 1_700_000_000_000);
        assert_eq!(a, b);
        assert_eq!(a.timestamp_ms(), 1_700_000_000_000);
        assert_ne!(a, artifact_node_id("shared", "b3:abd", 1_700_000_000_000));
        assert_ne!(a, artifact_node_id("other", "b3:abc", 1_700_000_000_000));
        let c0 = chunk_node_id(a, 0, 1, 1_700_000_000_000);
        assert_ne!(c0, chunk_node_id(a, 1, 1, 1_700_000_000_000));
        assert_ne!(c0, chunk_node_id(a, 0, 2, 1_700_000_000_000));
        let m = topodb::NodeId::new();
        assert_eq!(
            evidence_edge_id(m, a, "turn-window/1", 5),
            evidence_edge_id(m, a, "turn-window/1", 5)
        );
        assert_ne!(
            has_chunk_edge_id(a, c0, 5),
            evidence_edge_id(a, c0, "turn-window/1", 5)
        );
    }
}
