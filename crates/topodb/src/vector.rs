//! Vector search: the query type and the `Db::search_vector` read path.
//!
//! **Task 7 (format v4): the slab index is gone.** Through Task 6 this
//! module also owned a per-`(model, scope)` RAM slab index (`VectorIndex`/
//! `Slab`) that the applier maintained on every write and `search_vector`
//! read from, plus dim pre-validation (`VectorIndex::prevalidate_dims`) run
//! before each batch committed. Task 5 already cut `search_vector` over to
//! read the v4 clustered `vectors`/`embedding_ref` disk tables instead
//! (`vector_store::search_scan`), leaving the slab write-only; Task 7
//! finishes the job — the slab, its locking machinery, and its
//! poisoned-lock policy are deleted outright (see `db.rs`, which no longer
//! builds, maintains, or rebuilds any such index). Dim validation now lives
//! entirely in `storage.rs`'s `apply_op`/`check_or_pin_dim` (a permanent,
//! per-model — not per-`(model, scope)` — pin, transactional with the batch
//! that trips it), and the zero-dim-embedding guard moved into `apply_op`'s
//! `SetEmbedding` arm directly.
use crate::db::Db;
use crate::error::{storage_err, TopoError};
use crate::ids::{NodeId, ScopeSet};
use crate::state::NodeRecord;
use crate::storage::{read_node_by_slot, NODES};
use crate::vector_store::{search_scan, OrderedScore, EMBEDDING_REF, VECTORS};

/// A vector-search request: cosine-rank the embeddings under `model` within
/// `scopes` against `vector`, returning the top `k`.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub scopes: ScopeSet,
    pub model: String,
    pub vector: Vec<f32>,
    pub k: usize,
    /// Restrict scoring to these nodes (e.g. a traversal result). None = whole scope set.
    pub candidates: Option<Vec<NodeId>>,
}

impl Db {
    /// Cosine vector search under one `model`, scoped to `q.scopes`.
    ///
    /// `Rejected` if `q.k == 0` or `q.vector` is empty. Reads the v4
    /// clustered `vectors`/`embedding_ref` tables (`vector_store::search_scan`)
    /// inside ONE `begin_read` transaction that also resolves the winning
    /// slots straight to `NodeRecord`s via NODES/VECTORS/EMBEDDING_REF —
    /// mirrors `search_text`'s single-hop read (`fts.rs`), no separate
    /// snapshot.
    ///
    /// **Tie-break seam.** `search_scan` bounds each `(model, scope)`
    /// cluster's scan through a k-heap that conservatively retains ties at
    /// the boundary score rather than picking a winner by slot (creation)
    /// order — slot order is NOT `NodeId`/ULID order. This function applies
    /// the FINAL sort — score desc, `NodeId` asc — only AFTER every
    /// surviving slot has been resolved to its `NodeId`, and truncates to
    /// `k` only then. Doing the tie-break before resolution (i.e. inside the
    /// heap, by slot) would risk silently keeping the wrong side of a
    /// same-score tie.
    ///
    /// A resolved id storage no longer carries (removed between the scan and
    /// the resolve, both inside the same transaction so this is only a
    /// theoretical race with a concurrent writer's *next* transaction) is
    /// dropped — harmless. Result nodes are bumped (access counters).
    pub fn search_vector(&self, q: &VectorQuery) -> Result<Vec<(NodeRecord, f32)>, TopoError> {
        if q.k == 0 || q.vector.is_empty() {
            return Err(TopoError::Rejected(
                "vector search requires k > 0 and a non-empty query vector".into(),
            ));
        }

        let storage = self.storage();
        let tx = storage.db.begin_read().map_err(storage_err)?;
        let dicts = storage.dicts.read().expect("dict lock poisoned");
        let scope_registry = storage
            .scope_registry
            .read()
            .expect("scope registry lock poisoned");

        let hits = search_scan(&tx, &dicts, &scope_registry, q)?;

        let nodes = tx.open_table(NODES).map_err(storage_err)?;
        let vectors = tx.open_table(VECTORS).map_err(storage_err)?;
        let refs = tx.open_table(EMBEDDING_REF).map_err(storage_err)?;

        let mut out: Vec<(NodeRecord, f32)> = Vec::with_capacity(hits.len());
        for (slot, score) in hits {
            if let Some(rec) =
                read_node_by_slot(&nodes, &vectors, &refs, &dicts, &scope_registry, slot)?
            {
                // Defensive only, not load-bearing for isolation: `search_scan`
                // already restricts its scan to `q.scopes`'s own interned
                // scope ids (and, on the candidates fast path, checks the
                // resolved scope directly) — mirrors `search_text`'s
                // identical defensive re-check.
                if q.scopes.contains(rec.scope) {
                    out.push((rec, score));
                }
            }
        }
        out.sort_by(|a, b| {
            OrderedScore(b.1)
                .cmp(&OrderedScore(a.1))
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        out.truncate(q.k);
        self.bump(out.iter().map(|(n, _)| n.id));
        Ok(out)
    }
}
