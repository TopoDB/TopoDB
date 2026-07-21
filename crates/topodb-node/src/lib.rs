#[macro_use]
extern crate napi_derive;

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
}
