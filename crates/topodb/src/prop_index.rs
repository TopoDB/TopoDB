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
    let mut start = prefix.clone();
    start.extend_from_slice(&0u64.to_be_bytes());
    let mut end = prefix.clone();
    end.extend_from_slice(&u64::MAX.to_be_bytes());
    let mut out = Vec::new();
    for entry in table
        .range(start.as_slice()..=end.as_slice())
        .map_err(storage_err)?
    {
        let (k, _) = entry.map_err(storage_err)?;
        let key = k.value();
        // Load-bearing: a longer Str/Bytes value sharing `prefix` (e.g. "ab"
        // vs "abc") produces keys that fall INSIDE this byte range — only the
        // length check excludes them.
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

    /// Pins that the bounded range scan in `lookup` still excludes a longer
    /// value sharing the shorter value's byte prefix (e.g. "ab" vs "abc") —
    /// the `key.len() == prefix.len() + 8` guard is load-bearing, not the
    /// range bounds alone.
    #[test]
    fn lookup_excludes_longer_value_sharing_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("t.redb")).unwrap();
        let prop_key = 7u32;
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(PROP_INDEX).unwrap();
            table
                .insert(
                    index_key(prop_key, &IndexValue::Str("ab".into()), 1).as_slice(),
                    &[] as &[u8],
                )
                .unwrap();
            table
                .insert(
                    index_key(prop_key, &IndexValue::Str("abc".into()), 2).as_slice(),
                    &[] as &[u8],
                )
                .unwrap();
        }
        tx.commit().unwrap();

        let tx = db.begin_read().unwrap();
        let table = tx.open_table(PROP_INDEX).unwrap();
        assert_eq!(
            lookup(&table, prop_key, &IndexValue::Str("ab".into())).unwrap(),
            vec![1]
        );
        assert_eq!(
            lookup(&table, prop_key, &IndexValue::Str("abc".into())).unwrap(),
            vec![2]
        );
    }
}
