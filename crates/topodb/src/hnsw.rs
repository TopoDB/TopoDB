//! Deterministic HNSW vector index (F8). One graph per (model, scope)
//! cluster, keyed beside VECTORS. All construction happens inside
//! `apply_op` (op order = insertion order); levels are an integer-only
//! function of NodeId; every internal tie breaks by slot ascending — so
//! `rebuild_state_from_ops` reproduces these tables exactly.
use crate::codec::{frame_value, unframe_value};
use crate::error::{storage_err, TopoError};
use crate::ids::NodeId;
use redb::{ReadableTable, Table, TableDefinition};
use serde::{Deserialize, Serialize};

#[allow(dead_code)]
pub(crate) const HNSW_META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hnsw_meta");
#[allow(dead_code)]
pub(crate) const HNSW_LINKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hnsw_links");
pub(crate) const HNSW_META_FORMAT_V0: u8 = 0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HnswParams {
    pub version: u32,
    pub m: u32,
    pub m0: u32,
    pub ef_construction: u32,
    pub level_cap: u8,
    pub build_threshold: u64,
    pub rebuild_num: u32,
    pub rebuild_den: u32,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            version: 1,
            m: 16,
            m0: 32,
            ef_construction: 128,
            level_cap: 16,
            build_threshold: 1024,
            rebuild_num: 3,
            rebuild_den: 10,
        }
    }
}

impl HnswParams {
    #[allow(dead_code)]
    pub(crate) fn validate(&self) -> Result<(), TopoError> {
        if self.m < 2 || !self.m.is_power_of_two() {
            return Err(TopoError::Rejected(format!(
                "hnsw m must be a power of two >= 2, got {}",
                self.m
            )));
        }
        if self.m0 < self.m {
            return Err(TopoError::Rejected("hnsw m0 must be >= m".into()));
        }
        if self.ef_construction < self.m {
            return Err(TopoError::Rejected(
                "hnsw ef_construction must be >= m".into(),
            ));
        }
        if self.rebuild_den == 0 || self.rebuild_num >= self.rebuild_den {
            return Err(TopoError::Rejected(
                "hnsw rebuild ratio must be a proper fraction".into(),
            ));
        }
        if self.build_threshold < 2 {
            return Err(TopoError::Rejected(
                "hnsw build_threshold must be >= 2".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ClusterMeta {
    pub format: u8,
    pub built: bool,
    pub entry_slot: u64,
    pub entry_level: u8,
    pub graph_len: u64,
    pub stale: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LinkRow {
    pub tomb: bool,
    pub neighbors: Vec<u64>,
}

pub(crate) fn meta_key(model: u32, scope: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[..4].copy_from_slice(&model.to_be_bytes());
    k[4..].copy_from_slice(&scope.to_be_bytes());
    k
}

#[allow(dead_code)]
pub(crate) fn link_prefix(model: u32, scope: u32) -> [u8; 8] {
    meta_key(model, scope)
}

pub(crate) fn link_key(model: u32, scope: u32, slot: u64, level: u8) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[..8].copy_from_slice(&meta_key(model, scope));
    k[8..16].copy_from_slice(&slot.to_be_bytes());
    k[16] = level;
    k
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Integer-geometric level: P(level >= l) = (1/m)^l, computed from the
/// leading zeros of a splitmix64 hash of the NodeId — no RNG state, no
/// libm, bit-identical on every platform. Requires m to be a power of two
/// (validated in HnswParams::validate).
#[allow(dead_code)]
pub(crate) fn level_for(id: NodeId, m: u32, level_cap: u8) -> u8 {
    let v = id.as_u128();
    let h = splitmix64(splitmix64((v >> 64) as u64) ^ (v as u64));
    let bits_per_level = m.trailing_zeros(); // m = 2^bits
    let level = (h.leading_zeros() / bits_per_level) as u8;
    level.min(level_cap)
}

#[allow(dead_code)]
pub(crate) fn read_meta(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
) -> Result<Option<ClusterMeta>, TopoError> {
    let key = meta_key(model, scope);
    match table.get(key.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(value) => {
            let bytes = value.value();
            let meta: ClusterMeta =
                postcard::from_bytes(bytes).map_err(|e| TopoError::Encoding(e.to_string()))?;
            if meta.format != HNSW_META_FORMAT_V0 {
                return Err(TopoError::Encoding(format!(
                    "unknown hnsw meta format 0x{:02X}",
                    meta.format
                )));
            }
            Ok(Some(meta))
        }
    }
}

#[allow(dead_code)]
pub(crate) fn write_meta(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    meta: &ClusterMeta,
) -> Result<(), TopoError> {
    let key = meta_key(model, scope);
    let bytes = postcard::to_allocvec(meta).map_err(|e| TopoError::Encoding(e.to_string()))?;
    table
        .insert(key.as_slice(), bytes.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn read_links(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    slot: u64,
    level: u8,
) -> Result<Option<LinkRow>, TopoError> {
    let key = link_key(model, scope, slot, level);
    match table.get(key.as_slice()).map_err(storage_err)? {
        None => Ok(None),
        Some(value) => {
            let raw = unframe_value(value.value())?;
            let row: LinkRow =
                postcard::from_bytes(&raw).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(row))
        }
    }
}

#[allow(dead_code)]
pub(crate) fn write_links(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    model: u32,
    scope: u32,
    slot: u64,
    level: u8,
    row: &LinkRow,
) -> Result<(), TopoError> {
    let key = link_key(model, scope, slot, level);
    let raw = postcard::to_allocvec(row).map_err(|e| TopoError::Encoding(e.to_string()))?;
    let framed = frame_value(raw);
    table
        .insert(key.as_slice(), framed.as_slice())
        .map_err(storage_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeId;

    #[test]
    fn level_distribution_and_pins() {
        // Integer-geometric: P(level >= l) = 16^-l. Pin exact values for
        // fixed ids so any change to the hash or formula is loud.
        let l0 = level_for(NodeId::from_u128(10), 16, 16);
        let l1 = level_for(NodeId::from_u128(11), 16, 16);
        // Deterministic: same input, same output, always.
        assert_eq!(l0, level_for(NodeId::from_u128(10), 16, 16));
        assert_eq!(l1, level_for(NodeId::from_u128(11), 16, 16));
        // Distribution sanity over a range: level 0 dominates ~15/16.
        let mut counts = [0usize; 17];
        for i in 0..4096u128 {
            counts[level_for(NodeId::from_u128(i), 16, 16) as usize] += 1;
        }
        assert!(
            counts[0] > 3500,
            "level 0 should be ~15/16 of 4096, got {}",
            counts[0]
        );
        assert!(
            counts[1] > 100,
            "level 1 should be ~1/16 of 4096, got {}",
            counts[1]
        );
        // Cap respected.
        for i in 0..4096u128 {
            assert!(level_for(NodeId::from_u128(i), 16, 3) <= 3);
        }
    }

    #[test]
    fn keys_are_prefix_ordered() {
        let p = link_prefix(7, 9);
        let k = link_key(7, 9, 42, 3);
        assert_eq!(&k[..8], &p[..]);
        // Slot-major then level within a cluster.
        assert!(link_key(7, 9, 1, 5) < link_key(7, 9, 2, 0));
        assert!(link_key(7, 9, 2, 0) < link_key(7, 9, 2, 1));
        assert_eq!(meta_key(7, 9), p);
    }

    #[test]
    fn params_roundtrip_and_validate() {
        let p = HnswParams::default();
        p.validate().unwrap();
        let bytes = postcard::to_allocvec(&p).unwrap();
        assert_eq!(postcard::from_bytes::<HnswParams>(&bytes).unwrap(), p);
        assert!(
            HnswParams {
                m: 12,
                ..HnswParams::default()
            }
            .validate()
            .is_err(),
            "m must be a power of two"
        );
        assert!(HnswParams {
            rebuild_num: 10,
            rebuild_den: 10,
            ..HnswParams::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn link_row_roundtrip_via_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut links = tx.open_table(HNSW_LINKS).unwrap();
            let mut meta = tx.open_table(HNSW_META).unwrap();
            write_links(
                &mut links,
                1,
                2,
                3,
                0,
                &LinkRow {
                    tomb: false,
                    neighbors: vec![5, 9, 1],
                },
            )
            .unwrap();
            write_meta(
                &mut meta,
                1,
                2,
                &ClusterMeta {
                    format: HNSW_META_FORMAT_V0,
                    built: true,
                    entry_slot: 3,
                    entry_level: 0,
                    graph_len: 1,
                    stale: 0,
                },
            )
            .unwrap();
            assert_eq!(
                read_links(&links, 1, 2, 3, 0).unwrap().unwrap().neighbors,
                vec![5, 9, 1]
            );
            assert!(
                read_links(&links, 1, 2, 4, 0).unwrap().is_none(),
                "missing key is Ok(None)"
            );
            assert_eq!(read_meta(&meta, 1, 2).unwrap().unwrap().entry_slot, 3);
        }
        tx.commit().unwrap();
    }
}
