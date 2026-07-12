//! Small append-only registry mapping scopes to compact u32 ids for v3 rows.
use crate::error::{storage_err, TopoError};
use crate::ids::{Scope, ScopeId};
use crate::storage::scope_key;
use redb::{ReadableTable, Table, TableDefinition};
use std::collections::HashMap;
pub(crate) const SCOPES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("scopes");
fn key(id: u32) -> [u8; 4] {
    id.to_be_bytes()
}
fn scope_from_key(bytes: &[u8; 17]) -> Result<Scope, TopoError> {
    match bytes[0] {
        0 => {
            if bytes[1..].iter().any(|x| *x != 0) {
                Err(TopoError::Encoding("bad shared scope key".into()))
            } else {
                Ok(Scope::Shared)
            }
        }
        1 => Ok(Scope::Id(ScopeId::from_u128(u128::from_be_bytes(
            bytes[1..].try_into().unwrap(),
        )))),
        tag => Err(TopoError::Encoding(format!("bad scope key tag {tag}"))),
    }
}
#[derive(Default)]
pub(crate) struct ScopeRegistry {
    by_scope: HashMap<Scope, u32>,
    by_id: HashMap<u32, Scope>,
    next: u32,
}
impl ScopeRegistry {
    pub(crate) fn load(tx: &redb::ReadTransaction) -> Result<Self, TopoError> {
        match tx.open_table(SCOPES) {
            Ok(t) => Self::load_table_for_rebuild(&t),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(Self::default()),
            Err(e) => Err(storage_err(e)),
        }
    }
    pub(crate) fn load_table_for_rebuild(
        t: &impl ReadableTable<&'static [u8], &'static [u8]>,
    ) -> Result<Self, TopoError> {
        let mut out = Self::default();
        for item in t.iter().map_err(storage_err)? {
            let (k, v) = item.map_err(storage_err)?;
            let key_bytes: [u8; 4] = k
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad scope registry id".into()))?;
            let scope_bytes: [u8; 17] = v
                .value()
                .try_into()
                .map_err(|_| TopoError::Encoding("bad scope registry value".into()))?;
            let id = u32::from_be_bytes(key_bytes);
            let scope = scope_from_key(&scope_bytes)?;
            out.by_scope.insert(scope, id);
            out.by_id.insert(id, scope);
            out.next = out.next.max(
                id.checked_add(1)
                    .ok_or_else(|| TopoError::Encoding("scope id exhausted".into()))?,
            );
        }
        if out.by_id.get(&0) != Some(&Scope::Shared) {
            return Err(TopoError::Encoding(
                "scope registry missing shared row".into(),
            ));
        }
        Ok(out)
    }
    pub(crate) fn intern(
        &mut self,
        t: &mut Table<'_, &'static [u8], &'static [u8]>,
        scope: Scope,
    ) -> Result<u32, TopoError> {
        if let Some(id) = self.by_scope.get(&scope) {
            return Ok(*id);
        }
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| TopoError::Encoding("scope id exhausted".into()))?;
        t.insert(key(id).as_slice(), scope_key(scope).as_slice())
            .map_err(storage_err)?;
        self.by_scope.insert(scope, id);
        self.by_id.insert(id, scope);
        Ok(id)
    }
    pub(crate) fn resolve(&self, id: u32) -> Result<Scope, TopoError> {
        self.by_id
            .get(&id)
            .copied()
            .ok_or_else(|| TopoError::Encoding(format!("unknown scope id {id}")))
    }
    /// Read-only inverse of `resolve`: `scope`'s interned id, or `None` if
    /// this scope has never had a node/edge written under it (nothing to
    /// intern yet). Used by `search_text`'s read-only path, which cannot
    /// allocate a fresh scope id via `intern` (no write transaction) — a
    /// never-interned scope has no FTS rows either, so `None` is exactly the
    /// "skip this scope" signal callers need.
    pub(crate) fn id_of(&self, scope: Scope) -> Option<u32> {
        self.by_scope.get(&scope).copied()
    }
}
pub(crate) fn seed_shared(
    t: &mut Table<'_, &'static [u8], &'static [u8]>,
) -> Result<(), TopoError> {
    if t.get(key(0).as_slice()).map_err(storage_err)?.is_none() {
        t.insert(key(0).as_slice(), scope_key(Scope::Shared).as_slice())
            .map_err(storage_err)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use redb::Database;
    #[test]
    fn registry_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            seed_shared(&mut t).unwrap();
            let mut r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            assert_eq!(r.intern(&mut t, Scope::Shared).unwrap(), 0);
            let a = Scope::Id(ScopeId::from_u128(1));
            assert_eq!(r.intern(&mut t, a).unwrap(), 1);
            assert_eq!(r.resolve(1).unwrap(), a);
        }
        tx.commit().unwrap();
    }
}
