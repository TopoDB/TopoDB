use crate::error::TopoError;
use crate::op::Op;
use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;

pub const OPS: TableDefinition<u64, &[u8]> = TableDefinition::new("ops");
pub const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub const NODES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
pub const EDGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("edges");

pub const FORMAT_VERSION: u32 = 1;

pub struct Storage {
    pub(crate) db: Database,
}

impl Storage {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, TopoError> {
        let db = Database::create(path).map_err(redb::Error::from)?;
        let s = Self { db };
        // Ensure tables + format version exist.
        let tx = s.db.begin_write().map_err(redb::Error::from)?;
        {
            tx.open_table(OPS).map_err(redb::Error::from)?;
            tx.open_table(NODES).map_err(redb::Error::from)?;
            tx.open_table(EDGES).map_err(redb::Error::from)?;
            let mut meta = tx.open_table(META).map_err(redb::Error::from)?;
            if meta.get("format_version").map_err(redb::Error::from)?.is_none() {
                meta.insert("format_version", FORMAT_VERSION.to_le_bytes().as_slice())
                    .map_err(redb::Error::from)?;
            }
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok(s)
    }

    pub fn format_version(&self) -> Result<u32, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let meta = tx.open_table(META).map_err(redb::Error::from)?;
        let v = meta.get("format_version").map_err(redb::Error::from)?
            .ok_or_else(|| TopoError::Encoding("missing format_version".into()))?;
        let bytes: [u8; 4] = v.value().try_into()
            .map_err(|_| TopoError::Encoding("bad format_version".into()))?;
        Ok(u32::from_le_bytes(bytes))
    }

    pub fn append_ops(&self, ops: &[Op]) -> Result<(u64, u64), TopoError> {
        if ops.is_empty() {
            return Err(TopoError::Rejected("empty op batch".into()));
        }
        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        let (first, last);
        {
            let mut table = tx.open_table(OPS).map_err(redb::Error::from)?;
            let next = table.last().map_err(redb::Error::from)?
                .map(|(k, _)| k.value() + 1).unwrap_or(1);
            first = next;
            last = next + ops.len() as u64 - 1;
            for (i, op) in ops.iter().enumerate() {
                let bytes = postcard::to_allocvec(op)
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                table.insert(next + i as u64, bytes.as_slice())
                    .map_err(redb::Error::from)?;
            }
        }
        tx.commit().map_err(redb::Error::from)?;
        Ok((first, last))
    }

    pub fn read_ops(&self, since: u64) -> Result<Vec<(u64, Op)>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(OPS).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for entry in table.range(since..).map_err(redb::Error::from)? {
            let (k, v) = entry.map_err(redb::Error::from)?;
            let op: Op = postcard::from_bytes(v.value())
                .map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push((k.value(), op));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::*;
    use crate::op::Op;

    #[test]
    fn append_assigns_monotonic_seq_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::create(dir.path().join("t.redb")).unwrap();
        let scope = Scope::Id(ScopeId::new());
        let ops = vec![
            Op::CreateNode { id: NodeId::new(), scope, label: "Memory".into(), props: Default::default() },
            Op::CreateNode { id: NodeId::new(), scope, label: "Entity".into(), props: Default::default() },
        ];
        let (first, last) = s.append_ops(&ops).unwrap();
        assert_eq!((first, last), (1, 2));
        let read = s.read_ops(1).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].1, ops[0]);
        assert_eq!(s.format_version().unwrap(), 1);
    }

    #[test]
    fn append_ops_rejects_empty_batch() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::create(dir.path().join("t.redb")).unwrap();

        let err = s.append_ops(&[]).unwrap_err();
        assert!(matches!(err, TopoError::Rejected(_)));

        // Nothing was appended.
        assert!(s.read_ops(1).unwrap().is_empty());

        // A subsequent real append still starts at seq 1.
        let ops = vec![Op::CreateNode {
            id: NodeId::new(),
            scope: Scope::Id(ScopeId::new()),
            label: "Memory".into(),
            props: Default::default(),
        }];
        let (first, last) = s.append_ops(&ops).unwrap();
        assert_eq!((first, last), (1, 1));
    }
}
