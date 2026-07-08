use crate::error::TopoError;
use crate::ids::{EdgeId, NodeId, Scope};
use crate::op::Op;
use crate::state::{EdgeRecord, NodeRecord};
use redb::{Database, ReadableTable, Table, TableDefinition};
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

    /// Resolves defaults, validates, appends the resolved ops AND updates the
    /// NODES/EDGES state tables in one redb write transaction. On any
    /// validation failure nothing is committed and `TopoError::Rejected` is
    /// returned.
    pub fn apply_batch(&self, ops: Vec<Op>, now_ms: i64) -> Result<AppliedBatch, TopoError> {
        if ops.is_empty() {
            return Err(TopoError::Rejected("empty op batch".into()));
        }

        // Resolve defaults up front — the resolved op is what gets appended
        // and applied, so replay stays deterministic.
        let resolved: Vec<Op> = ops.into_iter().map(|op| resolve_op(op, now_ms)).collect();

        let tx = self.db.begin_write().map_err(redb::Error::from)?;
        {
            let mut nodes = tx.open_table(NODES).map_err(redb::Error::from)?;
            let mut edges = tx.open_table(EDGES).map_err(redb::Error::from)?;
            for op in &resolved {
                apply_op(&mut nodes, &mut edges, op)?;
            }
        }

        let (first_seq, last_seq);
        {
            let mut table = tx.open_table(OPS).map_err(redb::Error::from)?;
            let next = table
                .last()
                .map_err(redb::Error::from)?
                .map(|(k, _)| k.value() + 1)
                .unwrap_or(1);
            first_seq = next;
            last_seq = next + resolved.len() as u64 - 1;
            for (i, op) in resolved.iter().enumerate() {
                let bytes =
                    postcard::to_allocvec(op).map_err(|e| TopoError::Encoding(e.to_string()))?;
                table
                    .insert(next + i as u64, bytes.as_slice())
                    .map_err(redb::Error::from)?;
            }
        }

        tx.commit().map_err(redb::Error::from)?;
        Ok(AppliedBatch { first_seq, last_seq, resolved })
    }

    pub fn load_node(&self, id: NodeId) -> Result<Option<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(NODES).map_err(redb::Error::from)?;
        read_node(&table, id)
    }

    pub fn load_edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(EDGES).map_err(redb::Error::from)?;
        read_edge(&table, id)
    }

    /// Crate-internal full scan — used to rebuild in-memory adjacency. Not
    /// public API: callers should go through the (future) query layer.
    #[allow(dead_code)]
    pub(crate) fn all_nodes(&self) -> Result<Vec<NodeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(NODES).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(redb::Error::from)? {
            let (_, v) = entry.map_err(redb::Error::from)?;
            let rec: NodeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push(rec);
        }
        Ok(out)
    }

    #[allow(dead_code)]
    pub(crate) fn all_edges(&self) -> Result<Vec<EdgeRecord>, TopoError> {
        let tx = self.db.begin_read().map_err(redb::Error::from)?;
        let table = tx.open_table(EDGES).map_err(redb::Error::from)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(redb::Error::from)? {
            let (_, v) = entry.map_err(redb::Error::from)?;
            let rec: EdgeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            out.push(rec);
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub struct AppliedBatch {
    pub first_seq: u64,
    pub last_seq: u64,
    pub resolved: Vec<Op>,
}

/// Fills `CreateEdge.valid_from` / `CloseEdge.valid_to` with `Some(now_ms)`
/// where the caller left them `None`. All other variants pass through
/// unchanged. Idempotent: an already-resolved op (`Some(_)`) is left as-is.
fn resolve_op(op: Op, now_ms: i64) -> Op {
    match op {
        Op::CreateEdge { id, scope, ty, from, to, props, valid_from } => Op::CreateEdge {
            id,
            scope,
            ty,
            from,
            to,
            props,
            valid_from: Some(valid_from.unwrap_or(now_ms)),
        },
        Op::CloseEdge { id, valid_to } => {
            Op::CloseEdge { id, valid_to: Some(valid_to.unwrap_or(now_ms)) }
        }
        other => other,
    }
}

fn node_key(id: NodeId) -> [u8; 16] {
    id.0 .0.to_be_bytes()
}

fn edge_key(id: EdgeId) -> [u8; 16] {
    id.0 .0.to_be_bytes()
}

fn read_node(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: NodeId,
) -> Result<Option<NodeRecord>, TopoError> {
    let key = node_key(id);
    match table.get(key.as_slice()).map_err(redb::Error::from)? {
        None => Ok(None),
        Some(v) => {
            let rec: NodeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(rec))
        }
    }
}

fn read_edge(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
    id: EdgeId,
) -> Result<Option<EdgeRecord>, TopoError> {
    let key = edge_key(id);
    match table.get(key.as_slice()).map_err(redb::Error::from)? {
        None => Ok(None),
        Some(v) => {
            let rec: EdgeRecord =
                postcard::from_bytes(v.value()).map_err(|e| TopoError::Encoding(e.to_string()))?;
            Ok(Some(rec))
        }
    }
}

fn put_node(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    rec: &NodeRecord,
) -> Result<(), TopoError> {
    let key = node_key(rec.id);
    let bytes = postcard::to_allocvec(rec).map_err(|e| TopoError::Encoding(e.to_string()))?;
    table.insert(key.as_slice(), bytes.as_slice()).map_err(redb::Error::from)?;
    Ok(())
}

fn put_edge(
    table: &mut Table<'_, &'static [u8], &'static [u8]>,
    rec: &EdgeRecord,
) -> Result<(), TopoError> {
    let key = edge_key(rec.id);
    let bytes = postcard::to_allocvec(rec).map_err(|e| TopoError::Encoding(e.to_string()))?;
    table.insert(key.as_slice(), bytes.as_slice()).map_err(redb::Error::from)?;
    Ok(())
}

/// Applies a single (already-resolved) op to the NODES/EDGES tables,
/// validating against the current table state — which, mid-batch, already
/// reflects every earlier op in the same batch since we mutate the tables
/// incrementally within the one write transaction. Factored out so Task 7's
/// replay can reuse it without re-deriving the mutation logic.
fn apply_op(
    nodes: &mut Table<'_, &'static [u8], &'static [u8]>,
    edges: &mut Table<'_, &'static [u8], &'static [u8]>,
    op: &Op,
) -> Result<(), TopoError> {
    match op {
        Op::CreateNode { id, scope, label, props } => {
            let rec = NodeRecord {
                id: *id,
                scope: *scope,
                label: label.clone(),
                props: props.clone(),
                embedding: None,
            };
            put_node(nodes, &rec)
        }
        Op::SetNodeProps { id, props } => {
            let mut rec = read_node(nodes, *id)?.ok_or_else(|| {
                TopoError::Rejected(format!("SetNodeProps: node {id:?} not found"))
            })?;
            for (k, v) in props {
                match v {
                    Some(val) => {
                        rec.props.insert(k.clone(), val.clone());
                    }
                    None => {
                        rec.props.remove(k);
                    }
                }
            }
            put_node(nodes, &rec)
        }
        Op::SetEmbedding { id, model, vector } => {
            let mut rec = read_node(nodes, *id)?.ok_or_else(|| {
                TopoError::Rejected(format!("SetEmbedding: node {id:?} not found"))
            })?;
            rec.embedding = Some((model.clone(), vector.clone()));
            put_node(nodes, &rec)
        }
        Op::RemoveNode { id } => {
            let key = node_key(*id);
            nodes.remove(key.as_slice()).map_err(redb::Error::from)?;

            // Remove incident edges, both directions. v0.1: linear scan is
            // acceptable; adjacency-assisted delete arrives with Task 5.
            let mut incident = Vec::new();
            for entry in edges.iter().map_err(redb::Error::from)? {
                let (k, v) = entry.map_err(redb::Error::from)?;
                let rec: EdgeRecord = postcard::from_bytes(v.value())
                    .map_err(|e| TopoError::Encoding(e.to_string()))?;
                if rec.from == *id || rec.to == *id {
                    incident.push(k.value().to_vec());
                }
            }
            for key in incident {
                edges.remove(key.as_slice()).map_err(redb::Error::from)?;
            }
            Ok(())
        }
        Op::CreateEdge { id, scope, ty, from, to, props, valid_from } => {
            let from_rec = read_node(nodes, *from)?.ok_or_else(|| {
                TopoError::Rejected(format!("CreateEdge {id:?}: from node {from:?} not found"))
            })?;
            let to_rec = read_node(nodes, *to)?.ok_or_else(|| {
                TopoError::Rejected(format!("CreateEdge {id:?}: to node {to:?} not found"))
            })?;
            if from_rec.scope != to_rec.scope
                && from_rec.scope != Scope::Shared
                && to_rec.scope != Scope::Shared
            {
                return Err(TopoError::Rejected(format!(
                    "CreateEdge {id:?}: cross-scope edge requires at least one Shared endpoint"
                )));
            }
            let rec = EdgeRecord {
                id: *id,
                scope: *scope,
                ty: ty.clone(),
                from: *from,
                to: *to,
                props: props.clone(),
                valid_from: valid_from
                    .expect("apply_op only runs on resolved ops (valid_from filled by resolve_op)"),
                valid_to: None,
            };
            put_edge(edges, &rec)
        }
        Op::CloseEdge { id, valid_to } => {
            let mut rec = read_edge(edges, *id)?
                .ok_or_else(|| TopoError::Rejected(format!("CloseEdge: edge {id:?} not found")))?;
            if rec.valid_to.is_some() {
                return Err(TopoError::Rejected(format!("CloseEdge: edge {id:?} already closed")));
            }
            rec.valid_to = Some(
                valid_to
                    .expect("apply_op only runs on resolved ops (valid_to filled by resolve_op)"),
            );
            put_edge(edges, &rec)
        }
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
