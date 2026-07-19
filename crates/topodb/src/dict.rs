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
    /// v4: embedding model names, interned so `vector_dims`/`vectors` can key
    /// by a stable `u32` id rather than the raw string. Append-only registry
    /// — this kind was added at the END; never reorder or reuse a discriminant.
    Model = 3,
}
impl DictKind {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Label),
            1 => Some(Self::EdgeType),
            2 => Some(Self::PropKey),
            3 => Some(Self::Model),
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
    models: Map,
}
/// Records every id `Dicts::intern`/`ScopeRegistry::intern` newly allocate
/// during one fallible region (a batch's op loop + FTS edits), so a failure
/// anywhere in that region can undo exactly those allocations from the
/// in-memory mirrors — `Dicts::revert`/`ScopeRegistry::revert` below — rather
/// than paying an O(vocabulary) reload from disk on every batch, successful
/// or not. Entries that resolved to an ALREADY-interned id (the `Ok(id)`
/// early return in `intern`) are never journaled: nothing new was allocated,
/// so there is nothing to undo. `scope_ids` lives here too (rather than in
/// `scopes.rs`) so callers thread exactly one journal through both mirrors.
#[derive(Debug, Default)]
pub(crate) struct InternJournal {
    dict_ids: Vec<(DictKind, u32)>,
    pub(crate) scope_ids: Vec<u32>,
}
impl Dicts {
    fn map(&self, k: DictKind) -> &Map {
        match k {
            DictKind::Label => &self.labels,
            DictKind::EdgeType => &self.types,
            DictKind::PropKey => &self.props,
            DictKind::Model => &self.models,
        }
    }
    fn map_mut(&mut self, k: DictKind) -> &mut Map {
        match k {
            DictKind::Label => &mut self.labels,
            DictKind::EdgeType => &mut self.types,
            DictKind::PropKey => &mut self.props,
            DictKind::Model => &mut self.models,
        }
    }
    pub(crate) fn load(tx: &redb::ReadTransaction) -> Result<Self, TopoError> {
        match tx.open_table(DICT) {
            Ok(t) => Self::load_from_table(&t),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(Self::default()),
            Err(e) => Err(storage_err(e)),
        }
    }
    pub(crate) fn load_from_table(
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
        journal: &mut InternJournal,
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
        journal.dict_ids.push((kind, id));
        Ok(id)
    }
    /// Undoes exactly the ids `journal` recorded, in reverse allocation
    /// order. Because `intern` is only ever called from within ONE guarded
    /// write region at a time (the dict-mirror lock is held across the whole
    /// region, see `Storage::apply_batch`), each kind's ids were allocated
    /// strictly increasing during this journal's lifetime — so walking the
    /// journal backwards and resetting `next` to the id just removed always
    /// restores that kind's counter to its exact pre-region value, not just
    /// an upper bound.
    pub(crate) fn revert(&mut self, journal: &InternJournal) {
        for &(kind, id) in journal.dict_ids.iter().rev() {
            let m = self.map_mut(kind);
            if let Some(name) = m.ids.remove(&id) {
                m.names.remove(&name);
            }
            m.next = id;
        }
    }
    pub(crate) fn id_of(&self, kind: DictKind, value: &str) -> Option<u32> {
        self.map(kind).names.get(value).copied()
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
    /// Test-only: total entries across every namespace, so a white-box test
    /// can assert the mirror is back to its EXACT pre-batch shape after a
    /// revert (not just "not obviously larger").
    #[cfg(test)]
    fn total_len(&self) -> usize {
        self.labels.names.len()
            + self.types.names.len()
            + self.props.names.len()
            + self.models.names.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::Database;

    /// Pins the journal/revert mechanism itself (F9-11 Task 4): after
    /// interning a mix of brand-new AND already-known strings across
    /// multiple namespaces, reverting the journal must bring the mirror back
    /// to EXACTLY its pre-region entry count and next-id counters — not an
    /// approximation. This is the white-box net `apply_batch`'s
    /// `aborted_batch_leaves_no_phantom_interns` (tests/intern.rs) sits on
    /// top of.
    #[test]
    fn revert_restores_exact_pre_batch_counts_and_counters() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path().join("x.redb")).unwrap();
        let tx = db.begin_write().unwrap();
        let mut t = tx.open_table(DICT).unwrap();

        let mut dicts = Dicts::default();
        // Pre-seed some committed state, as if from a prior successful
        // batch, so revert has something non-empty to come back to.
        let mut seed_journal = InternJournal::default();
        dicts
            .intern(&mut t, DictKind::Label, "Memory", &mut seed_journal)
            .unwrap();
        dicts
            .intern(&mut t, DictKind::EdgeType, "RELATES_TO", &mut seed_journal)
            .unwrap();
        let pre_batch_total = dicts.total_len();
        let pre_batch_label_next = dicts.labels.next;
        let pre_batch_type_next = dicts.types.next;
        assert_eq!(pre_batch_total, 2);

        // A batch's worth of interning: two brand-new labels, a brand-new
        // prop key, a brand-new model, AND a re-intern of the already-known
        // "Memory" label (idempotent — must NOT be journaled, since nothing
        // new was allocated for it).
        let mut journal = InternJournal::default();
        let repeat_id = dicts
            .intern(&mut t, DictKind::Label, "Memory", &mut journal)
            .unwrap();
        dicts
            .intern(&mut t, DictKind::Label, "PhantomA", &mut journal)
            .unwrap();
        dicts
            .intern(&mut t, DictKind::Label, "PhantomB", &mut journal)
            .unwrap();
        dicts
            .intern(&mut t, DictKind::PropKey, "phantom_key", &mut journal)
            .unwrap();
        dicts
            .intern(&mut t, DictKind::Model, "phantom-model", &mut journal)
            .unwrap();
        assert_eq!(
            repeat_id,
            dicts.id_of(DictKind::Label, "Memory").unwrap(),
            "re-interning a known string must return its existing id"
        );
        assert_eq!(
            dicts.total_len(),
            pre_batch_total + 4,
            "4 NEW entries (2 labels, 1 prop key, 1 model); the repeat must not grow the mirror"
        );

        // Forced revert, as `apply_batch` does on any failure after interning
        // began.
        dicts.revert(&journal);

        assert_eq!(
            dicts.total_len(),
            pre_batch_total,
            "revert must restore the EXACT pre-batch entry count, not merely shrink it"
        );
        assert_eq!(
            dicts.id_of(DictKind::Label, "PhantomA"),
            None,
            "reverted label must be gone from the by-name map"
        );
        assert_eq!(
            dicts.id_of(DictKind::Label, "Memory"),
            Some(repeat_id),
            "the idempotent re-intern's target must survive revert unharmed"
        );
        assert_eq!(
            dicts.labels.next, pre_batch_label_next,
            "the label id counter must roll back too, or the on-disk row that \
             never saw these ids would be permanently skipped"
        );
        assert_eq!(dicts.types.next, pre_batch_type_next);

        // A subsequent, independent intern of the SAME string must allocate
        // a FRESH id from the rolled-back counter, not silently reuse
        // whatever the reverted (never-committed) id was.
        let mut journal2 = InternJournal::default();
        let fresh_id = dicts
            .intern(&mut t, DictKind::Label, "PhantomA", &mut journal2)
            .unwrap();
        assert_eq!(fresh_id, pre_batch_label_next);
    }
}
