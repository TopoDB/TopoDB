//! Versioned delta-varint adjacency block payloads for format v3.
use crate::error::TopoError;
use redb::ReadableTable;

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
    let mut last = None;
    for item in table.iter().map_err(crate::error::storage_err)? {
        let (key, _) = item.map_err(crate::error::storage_err)?;
        if let Some((found_slot, found_type, chunk)) = key_parts(key.value()) {
            if found_slot == slot && found_type == edge_type {
                last = Some(last.map_or(chunk, |previous: u32| previous.max(chunk)));
            }
        }
    }
    let chunk = last.unwrap_or(0);
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

pub(crate) fn adj_close(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    slot: u64,
    edge_type: u32,
    edge_slot: u64,
    valid_to: i64,
) -> Result<bool, TopoError> {
    let mut keys = Vec::new();
    for item in table.iter().map_err(crate::error::storage_err)? {
        let (key, _) = item.map_err(crate::error::storage_err)?;
        if key_parts(key.value()).is_some_and(|(s, ty, _)| s == slot && ty == edge_type) {
            keys.push(key.value().to_vec());
        }
    }
    for bytes in keys {
        let key: [u8; 16] = bytes.as_slice().try_into().expect("table key width");
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
    let mut keys = Vec::new();
    for item in table.iter().map_err(crate::error::storage_err)? {
        let (key, _) = item.map_err(crate::error::storage_err)?;
        if key_parts(key.value()).is_some_and(|(s, ty, _)| s == slot && ty == edge_type) {
            keys.push(key.value().to_vec());
        }
    }
    for bytes in keys {
        let key: [u8; 16] = bytes.as_slice().try_into().expect("table key width");
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
    let mut keys = Vec::new();
    for item in table.iter().map_err(crate::error::storage_err)? {
        let (key, _) = item.map_err(crate::error::storage_err)?;
        if key_parts(key.value()).is_some_and(|(s, _, _)| s == slot) {
            keys.push(key.value().to_vec());
        }
    }
    let mut entries = Vec::new();
    for bytes in keys {
        let key: [u8; 16] = bytes.as_slice().try_into().expect("table key width");
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
