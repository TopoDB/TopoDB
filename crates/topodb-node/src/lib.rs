#[macro_use]
extern crate napi_derive;

mod convert;
mod errors;

use napi::bindgen_prelude::*;
use std::sync::{Arc, Mutex};
use topodb::Db;

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

#[napi(js_name = "TopoDB")]
pub struct TopoDb {
    inner: Arc<Mutex<Option<Db>>>,
}

impl TopoDb {
    fn db(&self) -> Result<Db> {
        self.inner.lock().unwrap().clone().ok_or_else(errors::closed)
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
        Ok(TopoDb { inner: Arc::new(Mutex::new(Some(db))) })
    }

    #[napi(factory, js_name = "openWith")]
    pub async fn open_with(path: String, index_spec: serde_json::Value) -> Result<TopoDb> {
        let spec = serde_json::from_value::<topodb::IndexSpec>(index_spec)
            .map_err(|e| errors::rejected(format!("invalid index spec: {e}")))?;
        let db = blocking(move || Db::open_with(&path, spec)).await?;
        Ok(TopoDb { inner: Arc::new(Mutex::new(Some(db))) })
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
        let (ops, ids) =
            topodb_json::resolve_batch(&commands, scope).map_err(errors::rejected)?;
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
    pub async fn nodes_by_label(&self, scopes: Vec<String>, label: String) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nodes = blocking(move || Ok(db.nodes_by_label(&set, &label))).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByLabelNewest")]
    pub async fn nodes_by_label_newest(&self, scopes: Vec<String>, label: String, k: u32) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nodes = blocking(move || Ok(db.nodes_by_label_newest(&set, &label, k as usize))).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByProp")]
    pub async fn nodes_by_prop(&self, scopes: Vec<String>, label: String, prop: String, value: serde_json::Value) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let pv = convert::json_to_prop_value(&value)?;
        let nodes = blocking(move || db.nodes_by_prop(&set, &label, &prop, &pv)).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByPropNormalized")]
    pub async fn nodes_by_prop_normalized(&self, scopes: Vec<String>, label: String, prop: String, value: serde_json::Value) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let pv = convert::json_to_prop_value(&value)?;
        let nodes = blocking(move || db.nodes_by_prop_normalized(&set, &label, &prop, &pv)).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "nodesByFloatRange")]
    pub async fn nodes_by_float_range(&self, scopes: Vec<String>, prop: String, min: f64, max: f64) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let nodes = blocking(move || Ok(db.nodes_by_float_range(&set, &prop, min, max))).await?;
        convert::nodes_to_value(nodes)
    }

    #[napi(js_name = "edgesFrom")]
    pub async fn edges_from(&self, scopes: Vec<String>, from: String, opts: Option<EdgesFromOpts>) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let from_id = convert::parse_node_id(&from)?;
        let to_id = opts.as_ref().and_then(|o| o.to.as_ref()).map(|s| convert::parse_node_id(s)).transpose()?;
        let ty = opts.as_ref().and_then(|o| o.ty.clone());
        let open_only = opts.as_ref().and_then(|o| o.open_only).unwrap_or(false);
        let edges = blocking(move || db.edges_from(&set, from_id, to_id, ty.as_deref(), open_only)).await?;
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
    pub async fn traverse(&self, scopes: Vec<String>, seeds: Vec<String>, max_hops: u8, opts: Option<TraverseOpts>) -> Result<serde_json::Value> {
        let db = self.db()?;
        let set = convert::parse_scopes(&scopes)?;
        let seed_ids: Result<Vec<_>> = seeds.iter().map(|s| convert::parse_node_id(s)).collect();
        let seed_ids = seed_ids?;
        let edge_types = opts.as_ref().and_then(|o| o.edge_types.as_ref()).map(|ts| ts.iter().map(|t| t.into()).collect());
        let direction_str = opts.as_ref().and_then(|o| o.direction.as_ref()).map(|d| d.as_str()).unwrap_or("both");
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
}
