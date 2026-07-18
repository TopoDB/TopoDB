//! Host-side embedding lifecycle. The ENGINE never computes vectors
//! (principle 4) — this module is the host that does, feeding
//! SetEmbedding ops and recall query vectors. Everything here is
//! best-effort: a missing/failed model degrades every caller to
//! text-only behavior, never an error.
//!
//! Backend note: `fastembed`'s default `ort-download-binaries-*` backend has
//! no prebuilt ONNX Runtime binary for `x86_64-apple-darwin` (verified against
//! `ort-sys` 2.0.0-rc.12's binary distribution table — only
//! `aarch64-apple-darwin` is listed for macOS), so this crate depends on
//! `fastembed` with `ort-load-dynamic` instead of the download-binaries
//! default. That backend `dlopen`s a system/`ORT_DYLIB_PATH` ONNX Runtime at
//! *init* time rather than requiring one at *build* time — which fits this
//! module's contract perfectly: an environment without a usable ONNX Runtime
//! simply lands `Failed` (one stderr line), exactly like an unknown model
//! name or a network-less sandbox would.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const DEFAULT_MODEL: &str = "bge-small-en-v1.5";

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderStatus {
    Off,
    Downloading,
    Ready,
    Failed,
}

struct Inner {
    model_name: String,
    status: Mutex<EmbedderStatus>,
    engine: Mutex<Option<fastembed::TextEmbedding>>,
}

/// Shared handle; cheap to clone (an `Arc`). Interior state behind a mutex.
#[derive(Clone)]
pub struct Embedder {
    inner: Arc<Inner>,
}

/// Resolves a `--embeddings` model name string to the one `fastembed` model
/// this module currently wires up. Only [`DEFAULT_MODEL`] maps to a known
/// model today — anything else is a caller error we degrade rather than
/// reject: the server still starts, the embedder just lands `Failed`.
fn resolve_model(model_name: &str) -> Option<fastembed::EmbeddingModel> {
    if model_name == DEFAULT_MODEL {
        Some(fastembed::EmbeddingModel::BGESmallENV15)
    } else {
        None
    }
}

impl Embedder {
    /// Permanently `Off`: `--embeddings off`, or a host that never wants
    /// embeddings at all.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                model_name: DEFAULT_MODEL.into(),
                status: Mutex::new(EmbedderStatus::Off),
                engine: Mutex::new(None),
            }),
        }
    }

    /// Spawns a blocking init off the async runtime (first run downloads the
    /// model; later runs load it from `cache_dir`), transitioning
    /// `Downloading` -> `Ready` | `Failed`. `model` names anything other than
    /// [`DEFAULT_MODEL`] still constructs an `Embedder` — it just can't
    /// resolve to a known `fastembed` model, so it degrades straight to
    /// `Failed` with a clear stderr line instead of refusing to start the
    /// server.
    pub fn start(model: Option<String>, cache_dir: PathBuf) -> Self {
        let model_name = model.unwrap_or_else(|| DEFAULT_MODEL.into());
        let e = Self {
            inner: Arc::new(Inner {
                model_name: model_name.clone(),
                status: Mutex::new(EmbedderStatus::Downloading),
                engine: Mutex::new(None),
            }),
        };

        let Some(known_model) = resolve_model(&model_name) else {
            *e.inner.status.lock().unwrap() = EmbedderStatus::Failed;
            eprintln!(
                "topodb-mcp: unknown embedding model {model_name:?} (only {DEFAULT_MODEL:?} is \
                 currently wired up); running text-only"
            );
            return e;
        };

        let init = e.clone();
        // Blocking init off the async runtime: first run downloads the model;
        // later runs load from cache_dir. One stderr line per outcome — never
        // per-call spam. `catch_unwind` guards against a panic inside
        // fastembed/onnxruntime init (e.g. a corrupt cache file) reaching
        // across this thread boundary — a failed model must degrade to
        // `Failed`, never crash the process.
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                fastembed::TextEmbedding::try_new(
                    fastembed::TextInitOptions::new(known_model).with_cache_dir(cache_dir),
                )
            });
            match result {
                Ok(Ok(engine)) => {
                    *init.inner.engine.lock().unwrap() = Some(engine);
                    *init.inner.status.lock().unwrap() = EmbedderStatus::Ready;
                    eprintln!("topodb-mcp: embedding model {model_name} ready");
                }
                Ok(Err(err)) => {
                    *init.inner.status.lock().unwrap() = EmbedderStatus::Failed;
                    eprintln!(
                        "topodb-mcp: embedding model {model_name} unavailable ({err}); running text-only"
                    );
                }
                Err(_) => {
                    *init.inner.status.lock().unwrap() = EmbedderStatus::Failed;
                    eprintln!(
                        "topodb-mcp: embedding model {model_name} init panicked; running text-only"
                    );
                }
            }
        });
        e
    }

    pub fn status(&self) -> EmbedderStatus {
        *self.inner.status.lock().unwrap()
    }

    pub fn model_name(&self) -> String {
        self.inner.model_name.clone()
    }

    /// `None` unless `Ready`. Synchronous (callers run in tool handlers /
    /// backfill thread); ~10-40ms per text.
    ///
    /// Not yet called anywhere in this crate — write tools/search/backfill
    /// wiring is Task 11. `allow(dead_code)` here rather than on the whole
    /// module: every other method IS exercised (by `server.rs`'s `db_info`
    /// and this module's own tests).
    #[allow(dead_code)]
    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let mut guard = self.inner.engine.lock().unwrap();
        let engine = guard.as_mut()?;
        match engine.embed(vec![text], None) {
            Ok(mut vs) if !vs.is_empty() => Some(vs.remove(0)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_off_forever() {
        let e = Embedder::disabled();
        assert_eq!(e.status(), EmbedderStatus::Off);
        assert_eq!(e.model_name(), DEFAULT_MODEL);
        assert_eq!(e.embed("hello"), None);
    }

    #[test]
    fn unknown_model_degrades_to_failed_without_crashing() {
        let dir = tempfile::tempdir().unwrap();
        let e = Embedder::start(Some("not-a-real-model".into()), dir.path().to_path_buf());
        // The init "thread" for an unresolvable model name runs synchronously
        // (no fastembed/network call needed to know it's unresolvable), so
        // status is already terminal by the time `start` returns.
        assert_eq!(e.status(), EmbedderStatus::Failed);
        assert_eq!(e.embed("hello"), None);
    }
}
