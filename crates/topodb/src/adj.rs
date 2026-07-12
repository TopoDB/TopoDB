//! Versioned delta-varint adjacency block payloads for format v3.
use crate::error::TopoError;

pub(crate) const ADJ_BLOCK_FORMAT_V0: u8 = 0;
pub(crate) const CHUNK_SPLIT_TARGET: usize = 8 * 1024;
pub(crate) const OUT_ADJ: redb::TableDefinition<&[u8], &[u8]> =
    redb::TableDefinition::new("out_adj");
pub(crate) const IN_ADJ: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("in_adj");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdjEntryDisk {
    pub target: u64,
    pub edge: u64,
    pub scope: u32,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
}

pub(crate) fn out_adj_key(slot: u64, edge_type: u32, chunk: u32) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&slot.to_be_bytes());
    key[8..12].copy_from_slice(&edge_type.to_be_bytes());
    key[12..].copy_from_slice(&chunk.to_be_bytes());
    key
}

pub(crate) fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn read_varint(input: &mut &[u8]) -> Result<u64, TopoError> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let Some((&byte, rest)) = input.split_first() else {
            return Err(TopoError::Encoding("truncated adjacency varint".into()));
        };
        *input = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        if shift == 63 {
            break;
        }
    }
    Err(TopoError::Encoding("overflowing adjacency varint".into()))
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}
fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ (-((value & 1) as i64))
}

pub(crate) fn encode_block(entries: &[AdjEntryDisk]) -> Result<Vec<u8>, TopoError> {
    if entries
        .windows(2)
        .any(|pair| pair[0].target > pair[1].target)
    {
        return Err(TopoError::Encoding(
            "adjacency entries are not target-sorted".into(),
        ));
    }
    let mut out = Vec::new();
    out.push(ADJ_BLOCK_FORMAT_V0);
    write_varint(&mut out, entries.len() as u64);
    let mut previous = 0;
    for entry in entries {
        write_varint(
            &mut out,
            entry
                .target
                .checked_sub(previous)
                .ok_or_else(|| TopoError::Encoding("adjacency target underflow".into()))?,
        );
        previous = entry.target;
        write_varint(&mut out, entry.edge);
        write_varint(&mut out, entry.scope as u64);
        write_varint(&mut out, zigzag(entry.valid_from));
        match entry.valid_to {
            None => out.push(0),
            Some(valid_to) => {
                out.push(1);
                write_varint(&mut out, zigzag(valid_to));
            }
        }
    }
    Ok(out)
}

pub(crate) fn decode_block(payload: &[u8]) -> Result<Vec<AdjEntryDisk>, TopoError> {
    let Some((&format, mut input)) = payload.split_first() else {
        return Err(TopoError::Encoding("empty adjacency block".into()));
    };
    if format != ADJ_BLOCK_FORMAT_V0 {
        return Err(TopoError::Encoding(format!(
            "unknown adjacency block format 0x{format:02X}"
        )));
    }
    let count = usize::try_from(read_varint(&mut input)?)
        .map_err(|_| TopoError::Encoding("adjacency count too large".into()))?;
    let mut entries = Vec::with_capacity(count);
    let mut target = 0u64;
    for _ in 0..count {
        target = target
            .checked_add(read_varint(&mut input)?)
            .ok_or_else(|| TopoError::Encoding("adjacency target overflow".into()))?;
        let edge = read_varint(&mut input)?;
        let scope = u32::try_from(read_varint(&mut input)?)
            .map_err(|_| TopoError::Encoding("adjacency scope too large".into()))?;
        let valid_from = unzigzag(read_varint(&mut input)?);
        let Some((&tag, rest)) = input.split_first() else {
            return Err(TopoError::Encoding(
                "truncated adjacency valid_to tag".into(),
            ));
        };
        input = rest;
        let valid_to = match tag {
            0 => None,
            1 => Some(unzigzag(read_varint(&mut input)?)),
            _ => return Err(TopoError::Encoding("bad adjacency valid_to tag".into())),
        };
        entries.push(AdjEntryDisk {
            target,
            edge,
            scope,
            valid_from,
            valid_to,
        });
    }
    if !input.is_empty() {
        return Err(TopoError::Encoding(
            "trailing bytes in adjacency block".into(),
        ));
    }
    Ok(entries)
}

fn key_parts(key: &[u8]) -> Option<(u64, u32, u32)> {
    let bytes: [u8; 16] = key.try_into().ok()?;
    Some((
        u64::from_be_bytes(bytes[..8].try_into().ok()?),
        u32::from_be_bytes(bytes[8..12].try_into().ok()?),
        u32::from_be_bytes(bytes[12..].try_into().ok()?),
    ))
}

/// Chunk keys under `(slot, edge_type)`, in chunk order — a bounded range scan
/// over the 12-byte prefix, never a table iteration.
fn type_chunk_keys(
    table: &impl redb::ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
    edge_type: u32,
) -> Result<Vec<[u8; 16]>, TopoError> {
    let start = out_adj_key(slot, edge_type, 0);
    let end = out_adj_key(slot, edge_type, u32::MAX);
    let mut keys = Vec::new();
    for item in table
        .range(start.as_slice()..=end.as_slice())
        .map_err(crate::error::storage_err)?
    {
        let (key, _) = item.map_err(crate::error::storage_err)?;
        keys.push(key.value().try_into().expect("table key width"));
    }
    Ok(keys)
}

/// Chunk keys under `slot` (every edge type) — bounded by the 8-byte prefix.
fn slot_chunk_keys(
    table: &impl redb::ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Vec<[u8; 16]>, TopoError> {
    let start = out_adj_key(slot, 0, 0);
    let end = out_adj_key(slot, u32::MAX, u32::MAX);
    let mut keys = Vec::new();
    for item in table
        .range(start.as_slice()..=end.as_slice())
        .map_err(crate::error::storage_err)?
    {
        let (key, _) = item.map_err(crate::error::storage_err)?;
        keys.push(key.value().try_into().expect("table key width"));
    }
    Ok(keys)
}

fn load_chunk(
    table: &impl redb::ReadableTable<&'static [u8], &'static [u8]>,
    key: [u8; 16],
) -> Result<Vec<AdjEntryDisk>, TopoError> {
    match table
        .get(key.as_slice())
        .map_err(crate::error::storage_err)?
    {
        None => Ok(Vec::new()),
        Some(value) => {
            let raw = crate::codec::unframe_value(value.value())?;
            decode_block(raw.as_ref())
        }
    }
}

fn store_chunk(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    key: [u8; 16],
    entries: &[AdjEntryDisk],
) -> Result<(), TopoError> {
    let framed = crate::codec::frame_value(encode_block(entries)?);
    table
        .insert(key.as_slice(), framed.as_slice())
        .map_err(crate::error::storage_err)?;
    Ok(())
}

/// Inserts one entry, rewriting only the final chunk for this `(slot, type)`.
pub(crate) fn adj_insert(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    slot: u64,
    edge_type: u32,
    entry: AdjEntryDisk,
) -> Result<(), TopoError> {
    let chunk = type_chunk_keys(table, slot, edge_type)?
        .last()
        .and_then(|key| key_parts(key))
        .map(|(_, _, chunk)| chunk)
        .unwrap_or(0);
    let key = out_adj_key(slot, edge_type, chunk);
    let mut entries = load_chunk(table, key)?;
    let at = entries
        .binary_search_by_key(&(entry.target, entry.edge), |current| {
            (current.target, current.edge)
        })
        .unwrap_or_else(|index| index);
    entries.insert(at, entry);
    if encode_block(&entries)?.len() <= CHUNK_SPLIT_TARGET {
        store_chunk(table, key, &entries)
    } else {
        let split = entries.len() / 2;
        store_chunk(table, key, &entries[..split])?;
        store_chunk(
            table,
            out_adj_key(slot, edge_type, chunk + 1),
            &entries[split..],
        )
    }
}

/// Reads every adjacency entry stored under `slot`, optionally restricted to
/// `edge_types`, returning `(edge_type, entry)` pairs — the read path
/// (`read.rs`) needs the type alongside each entry (e.g. to build
/// `EdgeRecord`-shaped results downstream). With a type filter this is one
/// bounded range scan per requested type (`type_chunk_keys`, 12-byte
/// `(slot, type)` prefix); without one, a single bounded scan over the whole
/// `slot` prefix (`slot_chunk_keys`, 8 bytes), pulling each entry's type back
/// out of its chunk key via `key_parts`. Never a full-table iteration.
pub(crate) fn read_adj(
    table: &impl redb::ReadableTable<&'static [u8], &'static [u8]>,
    slot: u64,
    edge_types: Option<&[u32]>,
) -> Result<Vec<(u32, AdjEntryDisk)>, TopoError> {
    let mut out = Vec::new();
    match edge_types {
        Some(types) => {
            for &edge_type in types {
                for key in type_chunk_keys(table, slot, edge_type)? {
                    out.extend(load_chunk(table, key)?.into_iter().map(|e| (edge_type, e)));
                }
            }
        }
        None => {
            for key in slot_chunk_keys(table, slot)? {
                let (_, edge_type, _) = key_parts(&key)
                    .ok_or_else(|| TopoError::Encoding("bad adjacency key".into()))?;
                out.extend(load_chunk(table, key)?.into_iter().map(|e| (edge_type, e)));
            }
        }
    }
    Ok(out)
}

pub(crate) fn adj_close(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    slot: u64,
    edge_type: u32,
    edge_slot: u64,
    valid_to: i64,
) -> Result<bool, TopoError> {
    let keys = type_chunk_keys(table, slot, edge_type)?;
    for key in keys {
        let mut entries = load_chunk(table, key)?;
        if let Some(entry) = entries.iter_mut().find(|entry| entry.edge == edge_slot) {
            entry.valid_to = Some(valid_to);
            store_chunk(table, key, &entries)?;
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn adj_remove_edge(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    slot: u64,
    edge_type: u32,
    edge_slot: u64,
) -> Result<bool, TopoError> {
    let keys = type_chunk_keys(table, slot, edge_type)?;
    for key in keys {
        let mut entries = load_chunk(table, key)?;
        let before = entries.len();
        entries.retain(|entry| entry.edge != edge_slot);
        if entries.len() != before {
            if entries.is_empty() {
                table
                    .remove(key.as_slice())
                    .map_err(crate::error::storage_err)?;
            } else {
                store_chunk(table, key, &entries)?;
            }
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn adj_remove_all(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    slot: u64,
) -> Result<Vec<AdjEntryDisk>, TopoError> {
    let keys = slot_chunk_keys(table, slot)?;
    let mut entries = Vec::new();
    for key in keys {
        entries.extend(load_chunk(table, key)?);
        table
            .remove(key.as_slice())
            .map_err(crate::error::storage_err)?;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn entry(target: u64, edge: u64) -> AdjEntryDisk {
        AdjEntryDisk {
            target,
            edge,
            scope: 0,
            valid_from: 0,
            valid_to: None,
        }
    }

    /// Pins prefix isolation for the bounded range scans: `adj_remove_all`
    /// touches only its slot's chunks, and `adj_close` on one `(slot, type)`
    /// leaves an unrelated `(slot, type)` chunk's stored bytes untouched.
    #[test]
    fn range_scans_are_prefix_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();

        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(OUT_ADJ).unwrap();
            adj_insert(&mut table, 1, 1, entry(10, 100)).unwrap();
            adj_insert(&mut table, 1, 2, entry(20, 200)).unwrap();
            adj_insert(&mut table, 2, 1, entry(30, 300)).unwrap();
        }
        tx.commit().unwrap();

        let before_1_2 = {
            let tx = db.begin_read().unwrap();
            let table = tx.open_table(OUT_ADJ).unwrap();
            load_chunk(&table, out_adj_key(1, 2, 0)).unwrap()
        };

        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(OUT_ADJ).unwrap();
            assert!(adj_close(&mut table, 1, 1, 100, 999).unwrap());
        }
        tx.commit().unwrap();

        let after_1_2 = {
            let tx = db.begin_read().unwrap();
            let table = tx.open_table(OUT_ADJ).unwrap();
            load_chunk(&table, out_adj_key(1, 2, 0)).unwrap()
        };
        assert_eq!(
            before_1_2, after_1_2,
            "adj_close on (1,1) must not modify (1,2)'s stored bytes"
        );

        let tx = db.begin_write().unwrap();
        let removed = {
            let mut table = tx.open_table(OUT_ADJ).unwrap();
            adj_remove_all(&mut table, 1).unwrap()
        };
        tx.commit().unwrap();

        let mut removed_edges: Vec<u64> = removed.iter().map(|e| e.edge).collect();
        removed_edges.sort_unstable();
        assert_eq!(
            removed_edges,
            vec![100, 200],
            "adj_remove_all(slot=1) must return exactly the slot-1 entries, both types"
        );

        let tx = db.begin_read().unwrap();
        let table = tx.open_table(OUT_ADJ).unwrap();
        let slot2_chunk = load_chunk(&table, out_adj_key(2, 1, 0)).unwrap();
        assert_eq!(
            slot2_chunk.iter().map(|e| e.edge).collect::<Vec<_>>(),
            vec![300],
            "slot-2's chunk must remain present after adj_remove_all(slot=1)"
        );
    }

    #[test]
    fn roundtrips_boundaries_and_key() {
        let entries = vec![
            AdjEntryDisk {
                target: 0,
                edge: 1,
                scope: 0,
                valid_from: i64::MIN,
                valid_to: None,
            },
            AdjEntryDisk {
                target: 9,
                edge: 2,
                scope: u32::MAX,
                valid_from: i64::MAX,
                valid_to: Some(-1),
            },
        ];
        assert_eq!(
            decode_block(&encode_block(&entries).unwrap()).unwrap(),
            entries
        );
        assert_eq!(
            out_adj_key(1, 2, 3),
            [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        );
    }
    #[test]
    fn rejects_bad_payloads() {
        assert!(encode_block(&[
            AdjEntryDisk {
                target: 2,
                edge: 1,
                scope: 0,
                valid_from: 0,
                valid_to: None
            },
            AdjEntryDisk {
                target: 1,
                edge: 2,
                scope: 0,
                valid_from: 0,
                valid_to: None
            }
        ])
        .is_err());
        assert!(decode_block(&[]).is_err());
        assert!(decode_block(&[1]).is_err());
    }
    proptest! { #[test] fn sorted_entries_roundtrip(mut entries in proptest::collection::vec((0u64..10_000, 0u64..10_000, any::<u32>(), any::<i64>(), proptest::option::of(any::<i64>())), 0..64)) { entries.sort_by_key(|entry| entry.0); let entries: Vec<_> = entries.into_iter().enumerate().map(|(i, (target, _, scope, valid_from, valid_to))| AdjEntryDisk { target, edge: i as u64, scope, valid_from, valid_to }).collect(); prop_assert_eq!(decode_block(&encode_block(&entries).unwrap()).unwrap(), entries); } }
}
