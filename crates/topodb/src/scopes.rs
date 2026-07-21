//! Small append-only registry mapping scopes to compact u32 ids for v3 rows.
use crate::dict::InternJournal;
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
        journal: &mut InternJournal,
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
        journal.scope_ids.push(id);
        Ok(id)
    }
    /// See `Dicts::revert`: same reverse-order counter-restoration argument
    /// applies here (one registry, one monotonic counter, one guarded writer
    /// at a time).
    pub(crate) fn revert(&mut self, journal: &InternJournal) {
        for &id in journal.scope_ids.iter().rev() {
            if let Some(scope) = self.by_id.remove(&id) {
                self.by_scope.remove(&scope);
            }
            self.next = id;
        }
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
/// Whether the shared-scope row is already present, readable without a write
/// transaction. `open_with_options`'s read-only precheck needs to know
/// whether [`seed_shared`] would actually insert anything, and asking that
/// question must not itself require the write transaction it is trying to
/// avoid.
pub(crate) fn shared_is_seeded(
    t: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<bool, TopoError> {
    Ok(t.get(key(0).as_slice()).map_err(storage_err)?.is_some())
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
            let mut journal = InternJournal::default();
            assert_eq!(r.intern(&mut t, Scope::Shared, &mut journal).unwrap(), 0);
            let a = Scope::Id(ScopeId::from_u128(1));
            assert_eq!(r.intern(&mut t, a, &mut journal).unwrap(), 1);
            assert_eq!(r.resolve(1).unwrap(), a);
        }
        tx.commit().unwrap();
    }

    #[test]
    fn intern_same_scope_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            seed_shared(&mut t).unwrap();
            let mut r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            let mut journal = InternJournal::default();
            let a = Scope::Id(ScopeId::from_u128(7));
            let first = r.intern(&mut t, a, &mut journal).unwrap();
            let next_after_first = r.next;
            let second = r.intern(&mut t, a, &mut journal).unwrap();
            assert_eq!(first, second, "same scope must return the same id");
            assert_eq!(
                r.next, next_after_first,
                "re-interning an existing scope must not advance the next counter"
            );
        }
        tx.commit().unwrap();
    }

    #[test]
    fn intern_distinct_scopes_yield_increasing_ids() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            seed_shared(&mut t).unwrap();
            let mut r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            let mut journal = InternJournal::default();
            let scopes = [
                Scope::Shared,
                Scope::Id(ScopeId::from_u128(10)),
                Scope::Id(ScopeId::from_u128(20)),
                Scope::Id(ScopeId::from_u128(30)),
            ];
            let mut ids = Vec::new();
            for s in scopes {
                ids.push(r.intern(&mut t, s, &mut journal).unwrap());
            }
            for pair in ids.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "distinct scopes must yield strictly increasing ids, got {ids:?}"
                );
            }
            // Distinct ids overall.
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), ids.len(), "ids must all be distinct: {ids:?}");
        }
        tx.commit().unwrap();
    }

    #[test]
    fn revert_rolls_back_and_reuses_freed_id() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            seed_shared(&mut t).unwrap();
            let mut r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            // Seed the shared row in its own journal so it survives the revert.
            let mut seed_journal = InternJournal::default();
            r.intern(&mut t, Scope::Shared, &mut seed_journal).unwrap();
            let next_before = r.next;

            // Intern two distinct scopes in a fresh journal — these get reverted.
            let mut journal = InternJournal::default();
            let a = Scope::Id(ScopeId::from_u128(100));
            let b = Scope::Id(ScopeId::from_u128(200));
            let id_a = r.intern(&mut t, a, &mut journal).unwrap();
            let id_b = r.intern(&mut t, b, &mut journal).unwrap();
            assert!(r.next > next_before);

            r.revert(&journal);

            // Reverted ids gone from both directions of the maps.
            assert!(!r.by_id.contains_key(&id_a));
            assert!(!r.by_id.contains_key(&id_b));
            assert!(!r.by_scope.contains_key(&a));
            assert!(!r.by_scope.contains_key(&b));
            // Counter restored to where it was before the reverted interns.
            assert_eq!(r.next, next_before, "revert must restore the next counter");

            // A fresh scope reuses the lowest freed id.
            let mut journal2 = InternJournal::default();
            let c = Scope::Id(ScopeId::from_u128(300));
            let id_c = r.intern(&mut t, c, &mut journal2).unwrap();
            assert_eq!(id_c, id_a, "a new scope must reuse the freed id");
            assert_eq!(r.resolve(id_c).unwrap(), c);
        }
        tx.commit().unwrap();
    }

    #[test]
    fn resolve_returns_scope_and_errors_on_unknown_id() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            seed_shared(&mut t).unwrap();
            let mut r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            let mut journal = InternJournal::default();
            let a = Scope::Id(ScopeId::from_u128(42));
            let id = r.intern(&mut t, a, &mut journal).unwrap();
            assert_eq!(r.resolve(id).unwrap(), a);
            assert!(
                r.resolve(9999).is_err(),
                "resolving a never-interned id must return Err, not panic"
            );
        }
        tx.commit().unwrap();
    }

    #[test]
    fn id_of_none_then_some_after_intern() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            seed_shared(&mut t).unwrap();
            let mut r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            let a = Scope::Id(ScopeId::from_u128(55));
            assert_eq!(r.id_of(a), None, "never-interned scope must have no id");
            let mut journal = InternJournal::default();
            let id = r.intern(&mut t, a, &mut journal).unwrap();
            assert_eq!(
                r.id_of(a),
                Some(id),
                "after interning, id_of must return the interned id"
            );
        }
        tx.commit().unwrap();
    }

    #[test]
    fn seed_shared_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let db = Database::create(d.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut t = tx.open_table(SCOPES).unwrap();
            assert!(
                !shared_is_seeded(&t).unwrap(),
                "shared must not be seeded before seed_shared"
            );
            seed_shared(&mut t).unwrap();
            assert!(
                shared_is_seeded(&t).unwrap(),
                "shared must be seeded after seed_shared"
            );
            // Second call must be safe and must not double-seed.
            seed_shared(&mut t).unwrap();
            assert!(shared_is_seeded(&t).unwrap());
            let rows = t.iter().unwrap().count();
            assert_eq!(
                rows, 1,
                "seed_shared must not create a duplicate shared row"
            );
            // The single seeded row resolves back to Scope::Shared.
            let r = ScopeRegistry::load_table_for_rebuild(&t).unwrap();
            assert_eq!(r.resolve(0).unwrap(), Scope::Shared);
        }
        tx.commit().unwrap();
    }
}
