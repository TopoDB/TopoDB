use crossbeam_channel::{Receiver, RecvTimeoutError};
use napi::bindgen_prelude::*;
use std::sync::Mutex;
use std::time::Duration;
use topodb::ChangeEvent;

pub fn event_to_json(ev: &ChangeEvent) -> std::result::Result<serde_json::Value, String> {
    let op = serde_json::to_value(&*ev.op).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"seq": ev.seq, "op": op}))
}

#[napi]
pub struct Subscription {
    rx: Mutex<Option<Receiver<ChangeEvent>>>,
}

impl Subscription {
    pub fn new(rx: Receiver<ChangeEvent>) -> Self {
        Self {
            rx: Mutex::new(Some(rx)),
        }
    }
}

#[napi]
impl Subscription {
    /// Async next with optional timeout (milliseconds). None on timeout or disconnect.
    /// Disconnect ends iteration cleanly by returning Ok(None).
    #[napi]
    pub async fn next(&self, timeout_ms: Option<u32>) -> Result<Option<serde_json::Value>> {
        let rx = self
            .rx
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(crate::errors::closed)?;

        let got =
            tokio::task::spawn_blocking(move || -> std::result::Result<Option<ChangeEvent>, ()> {
                match timeout_ms {
                    Some(ms) => match rx.recv_timeout(Duration::from_millis(ms as u64)) {
                        Ok(ev) => Ok(Some(ev)),
                        Err(RecvTimeoutError::Timeout) => Ok(None),
                        Err(RecvTimeoutError::Disconnected) => Ok(None), // Clean end on disconnect
                    },
                    None => rx.recv().map(Some).or(Ok(None)), // Clean end on disconnect
                }
            })
            .await
            .map_err(|e| Error::from_reason(format!("[STORAGE] join error: {e}")))?;

        match got {
            Ok(Some(ev)) => {
                let j = event_to_json(&ev).map_err(crate::errors::rejected)?;
                Ok(Some(j))
            }
            Ok(None) => Ok(None),
            Err(_) => Ok(None), // Should not happen with our implementation above
        }
    }

    /// Close the subscription by dropping the receiver.
    #[napi]
    pub fn close(&self) {
        self.rx.lock().unwrap().take();
    }
}
