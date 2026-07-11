//! Append-only string dictionaries for v2 disk records.
use crate::error::{storage_err, TopoError};
use redb::{ReadableTable, Table, TableDefinition};
use smol_str::SmolStr;
use std::collections::HashMap;
pub(crate) const DICT: TableDefinition<&[u8], &str> = TableDefinition::new("dict");
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DictKind {
    Label = 0,
    EdgeType = 1,
    PropKey = 2,
}
impl DictKind {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Label),
            1 => Some(Self::EdgeType),
            2 => Some(Self::PropKey),
            _ => None,
        }
    }
}
fn key(kind: DictKind, id: u32) -> [u8; 5] {
    let mut k = [0; 5];
    k[0] = kind as u8;
    k[1..].copy_from_slice(&id.to_be_bytes());
    k
}
#[derive(Debug, Default)]
struct Map {
    names: HashMap<SmolStr, u32>,
    ids: HashMap<u32, SmolStr>,
    next: u32,
}
#[derive(Debug, Default)]
pub(crate) struct Dicts {
    labels: Map,
    types: Map,
    props: Map,
}
impl Dicts {
    fn map(&self, k: DictKind) -> &Map {
        match k {
            DictKind::Label => &self.labels,
            DictKind::EdgeType => &self.types,
            DictKind::PropKey => &self.props,
        }
    }
    fn map_mut(&mut self, k: DictKind) -> &mut Map {
        match k {
            DictKind::Label => &mut self.labels,
            DictKind::EdgeType => &mut self.types,
            DictKind::PropKey => &mut self.props,
        }
    }
    pub(crate) fn load(tx: &redb::ReadTransaction) -> Result<Self, TopoError> {
        match tx.open_table(DICT) {
            Ok(t) => Self::load_from_table(&t),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(Self::default()),
            Err(e) => Err(storage_err(e)),
        }
    }
    fn load_from_table(
        t: &impl ReadableTable<&'static [u8], &'static str>,
    ) -> Result<Self, TopoError> {
        let mut d = Self::default();
        for x in t.iter().map_err(storage_err)? {
            let (k, v) = x.map_err(storage_err)?;
            let k = k.value();
            if k.len() != 5 {
                return Err(TopoError::Encoding("bad dict key length".into()));
            }
            let kind = DictKind::from_byte(k[0])
                .ok_or_else(|| TopoError::Encoding("bad dict kind".into()))?;
            let id = u32::from_be_bytes(k[1..].try_into().unwrap());
            let name = SmolStr::new(v.value());
            let m = d.map_mut(kind);
            m.names.insert(name.clone(), id);
            m.ids.insert(id, name);
            m.next = m.next.max(
                id.checked_add(1)
                    .ok_or_else(|| TopoError::Encoding("dict id exhausted".into()))?,
            );
        }
        Ok(d)
    }
    pub(crate) fn intern(
        &mut self,
        t: &mut Table<'_, &'static [u8], &'static str>,
        kind: DictKind,
        s: &str,
    ) -> Result<u32, TopoError> {
        if let Some(id) = self.map(kind).names.get(s) {
            return Ok(*id);
        }
        let m = self.map_mut(kind);
        let id = m.next;
        m.next = m
            .next
            .checked_add(1)
            .ok_or_else(|| TopoError::Encoding("dict id exhausted".into()))?;
        t.insert(key(kind, id).as_slice(), s).map_err(storage_err)?;
        let n = SmolStr::new(s);
        m.names.insert(n.clone(), id);
        m.ids.insert(id, n);
        Ok(id)
    }
    pub(crate) fn resolve(&self, k: DictKind, id: u32) -> Result<SmolStr, TopoError> {
        self.map(k)
            .ids
            .get(&id)
            .cloned()
            .ok_or_else(|| TopoError::Encoding(format!("unknown {k:?} dict id {id}")))
    }
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
}
