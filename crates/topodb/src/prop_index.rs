//! On-disk equality index keyed by interned property key/value/slot.
use crate::dict::{DictKind, Dicts};
use crate::error::{storage_err, TopoError};
use crate::index::{IndexSpec, IndexValue};
use crate::state::NodeRecord;
use redb::{ReadableTable, Table, TableDefinition};

pub(crate) const PROP_INDEX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("prop_index");
fn value_bytes(value: &IndexValue, out: &mut Vec<u8>) {
    match value {
        IndexValue::Str(value) => {
            out.push(0);
            out.extend_from_slice(value.as_bytes());
        }
        IndexValue::Int(value) => {
            out.push(1);
            out.extend_from_slice(&((*value as u64) ^ (1 << 63)).to_be_bytes());
        }
        IndexValue::Bool(value) => {
            out.push(2);
            out.push(u8::from(*value));
        }
        IndexValue::Bytes(value) => {
            out.push(3);
            out.extend_from_slice(value);
        }
        IndexValue::DateTime(value) => {
            out.push(4);
            out.extend_from_slice(&((*value as u64) ^ (1 << 63)).to_be_bytes());
        }
    }
}
pub(crate) fn index_prefix(prop_key: u32, value: &IndexValue) -> Vec<u8> {
    let mut out = prop_key.to_be_bytes().to_vec();
    value_bytes(value, &mut out);
    out
}
pub(crate) fn index_key(prop_key: u32, value: &IndexValue, slot: u64) -> Vec<u8> {
    let mut out = index_prefix(prop_key, value);
    out.extend_from_slice(&slot.to_be_bytes());
    out
}
pub(crate) fn lookup(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    prop_key: u32,
    value: &IndexValue,
) -> Result<Vec<u64>, TopoError> {
    let prefix = index_prefix(prop_key, value);
    let mut out = Vec::new();
    for entry in table.iter().map_err(storage_err)? {
        let (k, _) = entry.map_err(storage_err)?;
        let key = k.value();
        if key.starts_with(&prefix) && key.len() == prefix.len() + 8 {
            out.push(u64::from_be_bytes(
                key[prefix.len()..].try_into().expect("slot suffix"),
            ));
        }
    }
    Ok(out)
}
fn should_index(spec: &IndexSpec, node: &NodeRecord, key: &str) -> bool {
    spec.equality
        .iter()
        .any(|item| item.label == node.label && item.prop == key)
}
pub(crate) fn index_node(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    spec: &IndexSpec,
    dicts: &Dicts,
    node: &NodeRecord,
    slot: u64,
) -> Result<(), TopoError> {
    for (key, value) in &node.props {
        if !should_index(spec, node, key) {
            continue;
        }
        let Some(prop) = dicts.id_of(DictKind::PropKey, key) else {
            continue;
        };
        let Some(value) = IndexValue::of(value) else {
            continue;
        };
        table
            .insert(index_key(prop, &value, slot).as_slice(), &[] as &[u8])
            .map_err(storage_err)?;
    }
    Ok(())
}
pub(crate) fn unindex_node(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    spec: &IndexSpec,
    dicts: &Dicts,
    node: &NodeRecord,
    slot: u64,
) -> Result<(), TopoError> {
    for (key, value) in &node.props {
        if !should_index(spec, node, key) {
            continue;
        }
        let Some(prop) = dicts.id_of(DictKind::PropKey, key) else {
            continue;
        };
        let Some(value) = IndexValue::of(value) else {
            continue;
        };
        table
            .remove(index_key(prop, &value, slot).as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_keys_sort() {
        assert!(index_key(1, &IndexValue::Int(-1), 0) < index_key(1, &IndexValue::Int(0), 0));
        assert!(index_key(1, &IndexValue::Int(0), 0) < index_key(1, &IndexValue::Int(1), 0));
    }
}
