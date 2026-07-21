#[macro_use]
extern crate napi_derive;

mod convert;
mod errors;
mod feed;

use napi::bindgen_prelude::*;
use std::sync::{Arc, Mutex};
use topodb::{Db, DbOptions};

#[napi(object)]
pub struct EdgesFromOpts {
    pub to: Option<String>,
    #[napi(js_name = "type")]
    pub ty: Option<String>,
    pub open_only: Option<bool>,
}

#[napi(object)]
pub struct TraverseOpts {
    pub edge_types: Option<Vec<String>>,
    pub direction: Option<String>,
    pub as_of: Option<i64>,
}

#[napi(object)]
pub struct SearchTextOpts {
    pub recency_weight: Option<f64>,
    pub recency_half_life_ms: Option<i64>,
    pub now_ms: Option<i64>,
}

#[napi(object)]
pub struct RecallVector {
    pub model: String,
    pub vector: Vec<f64>,
}

#[napi(object)]
pub struct RecallOpts {
    pub vector: Option<RecallVector>,
    pub expansions: Option<serde_json::Value>,
    pub graph_boost: Option<bool>,
    pub labels: Option<Vec<String>>,
    pub now_ms: Option<i64>,
}

#[napi(object)]
pub struct SuggestLinksOpts {
    pub model: Option<String>,
    pub as_of: Option<i64>,
    pub min_semantic_similarity: Option<f64>,
}

#[napi(js_name = "TopoDB")]
pub struct TopoDb {
    inner: Arc<Mutex<Option<Db>>>,
}

impl TopoDb {
    fn db(&self) -> Result<Db> {
        self.inner
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(errors::closed)
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::result::Result<T, topodb::TopoError> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::from_reason(format!("[STORAGE] join error: {e}")))?
        .map_err(errors::to_napi)
}

#[napi]
impl TopoDb {
    #[napi(factory)]
    pub async fn open(path: String) -> Result<TopoDb> {
        let db = blocking(move || Db::open(&path)).await?;
        Ok(TopoDb {
            inner: Arc::new(Mutex::new(Some(db))),
        })
    }

    #[napi(factory, js_name = "openWith")]
    pub async fn open_with(path: String, index_spec: serde_json::Value) -> Result<TopoDb> {
        let spec = serde_json::from_value::<topodb::IndexSpec>(index_spec)
            .map_err(|e| errors::rejected(format!("invalid index spec: {e}")))?;
        let db = blocking(move || Db::open_with(&path, spec)).await?;
        Ok(TopoDb {
            inner: Arc::new(Mutex::new(Some(db))),
        })
    }

    #[napi]
    pub async fn format_version(&self) -> Result<u32> {
        Ok(self.db()?.format_version())
    }

    #[napi]
    pub fn close(&self) {
        self.inner.lock().unwrap().take();
    }

    #[napi]
    pub async fn submit(
        &self,
        commands: serde_json::Value,
        default_scope: Option<String>,
        now_ms: Option<i64>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let scope = convert::parse_scope(default_scope.as_deref())?;
        let (ops, ids) = topodb_json::resolve_batch(&commands, scope).map_err(errors::rejected)?;
        let applied = blocking(move || match now_ms {
            Some(t) => db.submit_at(ops, t),
            None => db.submit(ops),
        })
        .await?;
        Ok(serde_json::json!({
            "firstSeq": applied.first_seq,
            "lastSeq": applied.last_seq,
            "ids": ids,
        }))
    }

    #[napi(js_name = "node")]
    pub async fn node(&self, scopes: Vec<String>, id: String) -> Result<Option<serde_json::Value>> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nid = convert::parse_node_id(&id)?;
        let n = blocking(move || Ok(db.node(&set, nid))).await?;
        n.map(|nr| convert::node_to_value(&nr)).transpose()
    }

    #[napi(js_name = "nodesByLabel")]
    pub async fn nodes_by_label(
        &self,
        scopes: Vec<String>,
        label: String,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nodes = blocking(move || Ok(db.nodes_by_label(&set, &label))).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByLabelNewest")]
    pub async fn nodes_by_label_newest(
        &self,
        scopes: Vec<String>,
        label: String,
        k: u32,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nodes =
            blocking(move || Ok(db.nodes_by_label_newest(&set, &label, k as usize))).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByProp")]
    pub async fn nodes_by_prop(
        &self,
        scopes: Vec<String>,
        label: String,
        prop: String,
        value: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let pv = convert::json_to_prop_value(&value)?;
        let nodes = blocking(move || db.nodes_by_prop(&set, &label, &prop, &pv)).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByPropNormalized")]
    pub async fn nodes_by_prop_normalized(
        &self,
        scopes: Vec<String>,
        label: String,
        prop: String,
        value: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let pv = convert::json_to_prop_value(&value)?;
        let nodes = blocking(move || db.nodes_by_prop_normalized(&set, &label, &prop, &pv)).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByFloatRange")]
    pub async fn nodes_by_float_range(
        &self,
        scopes: Vec<String>,
        prop: String,
        min: f64,
        max: f64,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nodes = blocking(move || Ok(db.nodes_by_float_range(&set, &prop, min, max))).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "edgesFrom")]
    pub async fn edges_from(
        &self,
        scopes: Vec<String>,
        from: String,
        opts: Option<EdgesFromOpts>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let from_id = convert::parse_node_id(&from)?;
        let to_id = opts
            .as_ref()
            .and_then(|o| o.to.as_ref())
            .map(|s| convert::parse_node_id(s))
            .transpose()?;
        let ty = opts.as_ref().and_then(|o| o.ty.clone());
        let open_only = opts.as_ref().and_then(|o| o.open_only).unwrap_or(false);
        let edges =
            blocking(move || db.edges_from(&set, from_id, to_id, ty.as_deref(), open_only)).await?;
        convert::edges_to_value(edges)
    }

    #[napi(js_name = "allEdgesBetween")]
    pub async fn all_edges_between(&self, from: String, to: String) -> Result<serde_json::Value> {
        let db = self.db()?;
        let from_id = convert::parse_node_id(&from)?;
        let to_id = convert::parse_node_id(&to)?;
        let edges = blocking(move || Ok(db.all_edges_between(from_id, to_id))).await?;
        convert::edges_to_value(edges)
    }

    #[napi(js_name = "openEdgesBetween")]
    pub async fn open_edges_between(&self, from: String, to: String) -> Result<Vec<String>> {
        let db = self.db()?;
        let from_id = convert::parse_node_id(&from)?;
        let to_id = convert::parse_node_id(&to)?;
        let edge_ids = blocking(move || Ok(db.open_edges_between(from_id, to_id))).await?;
        Ok(edge_ids.iter().map(|e| e.to_string()).collect())
    }

    #[napi(js_name = "traverse")]
    pub async fn traverse(
        &self,
        scopes: Vec<String>,
        seeds: Vec<String>,
        max_hops: u8,
        opts: Option<TraverseOpts>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let seed_ids: Result<Vec<_>> = seeds.iter().map(|s| convert::parse_node_id(s)).collect();
        let seed_ids = seed_ids?;
        let edge_types = opts
            .as_ref()
            .and_then(|o| o.edge_types.as_ref())
            .map(|ts| ts.iter().map(|t| t.into()).collect());
        let direction_str = opts
            .as_ref()
            .and_then(|o| o.direction.as_ref())
            .map(|d| d.as_str())
            .unwrap_or("both");
        let direction = convert::parse_direction(direction_str)?;
        let as_of = opts.as_ref().and_then(|o| o.as_of);
        let q = topodb::TraversalQuery {
            scopes: set,
            seeds: seed_ids,
            max_hops,
            edge_types,
            direction,
            as_of,
        };
        let sg = blocking(move || db.traverse(&q)).await?;
        convert::subgraph_to_value(&sg)
    }

    #[napi(js_name = "searchText")]
    pub async fn search_text(
        &self,
        scopes: Vec<String>,
        query: String,
        k: u32,
        opts: Option<SearchTextOpts>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let recency_weight = opts.as_ref().and_then(|o| o.recency_weight).unwrap_or(0.0) as f32;
        let recency_half_life_ms = opts
            .as_ref()
            .and_then(|o| o.recency_half_life_ms)
            .unwrap_or(0);
        let now_ms = opts.as_ref().and_then(|o| o.now_ms);
        let options = topodb::SearchOptions {
            recency_weight,
            recency_half_life_ms,
            now_ms,
            ..Default::default()
        };
        let hits =
            blocking(move || db.search_text_with(&set, &query, k as usize, &options)).await?;
        convert::scored_to_value(hits)
    }

    #[napi(js_name = "searchVector")]
    pub async fn search_vector(
        &self,
        scopes: Vec<String>,
        model: String,
        vector: Vec<f64>,
        k: u32,
        candidates: Option<Vec<String>>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let vector_f32: Vec<f32> = vector.into_iter().map(|v| v as f32).collect();
        let candidates_ids: Option<Result<Vec<_>>> =
            candidates.map(|cs| cs.iter().map(|s| convert::parse_node_id(s)).collect());
        let candidates_ids = candidates_ids.transpose()?;
        let q = topodb::VectorQuery {
            scopes: set,
            model,
            vector: vector_f32,
            k: k as usize,
            candidates: candidates_ids,
        };
        let hits = blocking(move || db.search_vector(&q)).await?;
        convert::scored_to_value(hits)
    }

    #[napi(js_name = "recall")]
    pub async fn recall(
        &self,
        scopes: Vec<String>,
        query: String,
        k: u32,
        opts: Option<RecallOpts>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let mut q = topodb::RecallQuery::new(set, query, k as usize);

        // Always set graph_boost explicitly with false default (match Python behavior)
        q.graph_boost = opts.as_ref().and_then(|o| o.graph_boost).unwrap_or(false);

        if let Some(opts) = opts {
            if let Some(vec) = opts.vector {
                let vector_f32: Vec<f32> = vec.vector.into_iter().map(|v| v as f32).collect();
                q.vector = Some((vec.model, vector_f32));
            }
            if let Some(exp_val) = opts.expansions {
                // Parse expansions from JSON: [[term, [alt1, alt2, ...]], ...]
                q.expansions = serde_json::from_value::<Vec<(String, Vec<String>)>>(exp_val)
                    .map_err(|e| errors::rejected(format!("invalid expansions: {e}")))?;
            }
            q.labels = opts.labels;
            q.options.now_ms = opts.now_ms;
        }

        let hits = blocking(move || db.recall(&q)).await?;
        convert::scored_to_value(hits)
    }

    #[napi(js_name = "suggestLinks")]
    pub async fn suggest_links(
        &self,
        scopes: Vec<String>,
        node: String,
        k: u32,
        opts: Option<SuggestLinksOpts>,
    ) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nid = convert::parse_node_id(&node)?;
        let min_sim = opts
            .as_ref()
            .and_then(|o| o.min_semantic_similarity)
            .map(|v| v as f32);
        let q = topodb::SuggestLinksQuery {
            scopes: set,
            node: nid,
            k: k as usize,
            model: opts.as_ref().and_then(|o| o.model.clone()),
            as_of: opts.as_ref().and_then(|o| o.as_of),
            min_semantic_similarity: min_sim,
        };
        let out = blocking(move || db.suggest_links(&q)).await?;
        let mut rows = Vec::with_capacity(out.len());
        for s in &out {
            let node = topodb_json::node_to_json(&s.node).map_err(Error::from_reason)?;
            rows.push(serde_json::json!({
                "node": node,
                "score": s.score,
                "commonNeighbors": s.common_neighbors.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                "structural": s.structural,
                "semantic": s.semantic,
            }));
        }
        Ok(serde_json::Value::Array(rows))
    }

    /// Subscribe to change feed with given buffer capacity.
    #[napi]
    pub fn subscribe(&self, capacity: u32) -> Result<feed::Subscription> {
        let db = self.db()?;
        Ok(feed::Subscription::new(db.subscribe(capacity as usize)))
    }

    /// Get all operations since a given sequence number.
    #[napi(js_name = "opsSince")]
    pub async fn ops_since(&self, seq: i64) -> Result<Vec<serde_json::Value>> {
        let db = self.db()?;
        let evs = blocking(move || db.ops_since(seq as u64)).await?;
        let rows: Result<Vec<_>, _> = evs
            .iter()
            .map(|ev| feed::event_to_json(ev).map_err(errors::rejected))
            .collect();
        rows
    }

    /// Get the current sequence number.
    #[napi(js_name = "currentSeq")]
    pub async fn current_seq(&self) -> Result<i64> {
        let db = self.db()?;
        let seq = blocking(move || db.current_seq()).await?;
        Ok(seq as i64)
    }

    /// Compact operations, keeping from the given sequence.
    #[napi(js_name = "compactOps")]
    pub async fn compact_ops(&self, keep_from: i64) -> Result<()> {
        let db = self.db()?;
        blocking(move || db.compact_ops(keep_from as u64)).await
    }

    /// Get the index specification.
    #[napi(js_name = "indexSpec")]
    pub async fn index_spec(&self) -> Result<serde_json::Value> {
        let db = self.db()?;
        let spec = blocking(move || Ok(db.index_spec())).await?;
        serde_json::to_value(spec).map_err(|e| errors::rejected(e.to_string()))
    }

    /// Get storage report with table statistics.
    #[napi(js_name = "storageReport")]
    pub async fn storage_report(&self) -> Result<Vec<serde_json::Value>> {
        let db = self.db()?;
        let report = blocking(move || db.storage_report()).await?;
        let rows: Vec<_> = report
            .iter()
            .map(|r| {
                serde_json::json!({
                    "table": r.table,
                    "rows": r.rows,
                    "keyBytes": r.key_bytes,
                    "valueBytes": r.value_bytes,
                })
            })
            .collect();
        Ok(rows)
    }

    /// Get access statistics for a node. Returns null if node doesn't exist.
    /// Returns {accessCount, lastAccessedAt} where accessCount is 0 for never-accessed existing nodes.
    #[napi(js_name = "accessStats")]
    pub async fn access_stats(
        &self,
        scopes: Vec<String>,
        id: String,
    ) -> Result<Option<serde_json::Value>> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nid = convert::parse_node_id(&id)?;
        let stats = blocking(move || db.access_stats(&set, nid)).await?;
        Ok(stats.map(|s| {
            serde_json::json!({
                "accessCount": s.access_count,
                "lastAccessedAt": s.last_accessed_at,
            })
        }))
    }

    /// Rebuild state from operations (replay ops log).
    #[napi(js_name = "rebuildStateFromOps")]
    pub async fn rebuild_state_from_ops(&self) -> Result<()> {
        let db = self.db()?;
        blocking(move || db.rebuild_state_from_ops()).await
    }

    /// Open an existing database from storage.
    #[napi(factory, js_name = "openStored")]
    pub async fn open_stored(path: String) -> Result<TopoDb> {
        let db = blocking(move || Db::open_stored(&path)).await?;
        Ok(TopoDb {
            inner: Arc::new(Mutex::new(Some(db))),
        })
    }

    /// Open database with options (index spec and optional cache size).
    #[napi(factory, js_name = "openWithOptions")]
    pub async fn open_with_options(
        path: String,
        index_spec: serde_json::Value,
        cache_size_bytes: Option<u32>,
    ) -> Result<TopoDb> {
        let spec = serde_json::from_value::<topodb::IndexSpec>(index_spec)
            .map_err(|e| errors::rejected(format!("invalid index spec: {e}")))?;
        let opts = DbOptions {
            cache_size_bytes: cache_size_bytes.map(|s| s as usize),
        };
        let db = blocking(move || Db::open_with_options(&path, spec, opts)).await?;
        Ok(TopoDb {
            inner: Arc::new(Mutex::new(Some(db))),
        })
    }

    /// Unstable debug surface — shape may change without notice.
    #[napi(js_name = "debugDumpNodes")]
    pub async fn debug_dump_nodes(&self) -> Result<Vec<serde_json::Value>> {
        let db = self.db()?;
        let nodes = blocking(move || Ok(db.debug_dump_nodes())).await?;
        convert::nodes_to_value(nodes).map(|v| match v {
            serde_json::Value::Array(arr) => arr,
            _ => vec![],
        })
    }

    /// Unstable debug surface — shape may change without notice.
    #[napi(js_name = "debugDumpEdges")]
    pub async fn debug_dump_edges(&self) -> Result<Vec<serde_json::Value>> {
        let db = self.db()?;
        let edges = blocking(move || Ok(db.debug_dump_edges())).await?;
        convert::edges_to_value(edges).map(|v| match v {
            serde_json::Value::Array(arr) => arr,
            _ => vec![],
        })
    }
}
