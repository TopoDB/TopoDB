#[macro_use]
extern crate napi_derive;

mod convert;
mod errors;

use napi::bindgen_prelude::*;
use std::sync::{Arc, Mutex};
use topodb::Db;

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
}
