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
    /// server. `ort_download` gates whether the init thread's ONNX Runtime
    /// resolver (`ort_fetch::resolve`) may download a runtime on a miss —
    /// `false` for `--no-ort-download`.
    pub fn start(model: Option<String>, cache_dir: PathBuf, ort_download: bool) -> Self {
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
            // Resolve an ONNX Runtime BEFORE any call into ort (the
            // pykeio/ort#604 deadlock defense now lives inside the
            // resolver's probes — see `ort_fetch::probe_loadable`). May
            // download on first run — that is why this runs here, under the
            // Downloading status, off the async runtime.
            match crate::ort_fetch::resolve(&cache_dir, ort_download, &crate::ort_fetch::http_fetch)
            {
                crate::ort_fetch::OrtRuntime::EnvOverride
                | crate::ort_fetch::OrtRuntime::System => {}
                crate::ort_fetch::OrtRuntime::Local(dylib) => {
                    // Point ort at the cached/downloaded dylib
                    // programmatically — no env mutation (racy) needed.
                    // `init_from` returns `Result` (the dylib load itself,
                    // matching the resolver's own probe); `commit()` on the
                    // resulting builder is infallible (`bool`). Both are
                    // wrapped in `catch_unwind`: ort's dylib-load path can
                    // panic internally (e.g. an `assert!` after
                    // `OrtGetApiBase`), and an uncaught panic here would kill
                    // this thread without ever setting `Failed`, leaving
                    // `db_info` reporting `Downloading` forever instead of
                    // degrading to text-only.
                    let init_result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            ort::init_from(&dylib).map(|builder| {
                                // `false` only means an environment was
                                // already configured elsewhere, which is
                                // harmless here — this is the first ort call
                                // on this thread and no env options are set.
                                let _ = builder.commit();
                            })
                        }));
                    match init_result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            *init.inner.status.lock().unwrap() = EmbedderStatus::Failed;
                            eprintln!(
                                "topodb-mcp: embedding model {model_name} unavailable (ort \
                                 init from {} failed: {e}); running text-only",
                                dylib.display()
                            );
                            return;
                        }
                        Err(_) => {
                            *init.inner.status.lock().unwrap() = EmbedderStatus::Failed;
                            eprintln!(
                                "topodb-mcp: embedding model {model_name} unavailable (ort \
                                 init from {} panicked); running text-only",
                                dylib.display()
                            );
                            return;
                        }
                    }
                }
                crate::ort_fetch::OrtRuntime::Unavailable(reason) => {
                    *init.inner.status.lock().unwrap() = EmbedderStatus::Failed;
                    eprintln!(
                        "topodb-mcp: embedding model {model_name} unavailable ({reason}); \
                         running text-only"
                    );
                    return;
                }
            }
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
    /// backfill thread); ~10-40ms per text. Called by `server.rs`'s
    /// `embed_op` (write tools) and by the search/recall tools' vector leg,
    /// plus the startup backfill pass.
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
        let e = Embedder::start(
            Some("not-a-real-model".into()),
            dir.path().to_path_buf(),
            true,
        );
        // The init "thread" for an unresolvable model name runs synchronously
        // (no fastembed/network call needed to know it's unresolvable), so
        // status is already terminal by the time `start` returns.
        assert_eq!(e.status(), EmbedderStatus::Failed);
        assert_eq!(e.embed("hello"), None);
    }

    /// Real network + ~50MB download + model fetch. Run explicitly:
    /// `cargo test -p topodb-mcp real_ort_download -- --ignored --nocapture`
    /// Precedent: the ignored real-model e2e tests. Asserts the F3 headline:
    /// a clean cache dir on a host with NO system ORT still reaches Ready.
    /// (On a host WITH a system ORT this passes trivially via the System
    /// path — still a valid lifecycle check, just weaker.)
    #[test]
    #[ignore]
    fn real_ort_download_reaches_ready() {
        let dir = tempfile::tempdir().unwrap();
        let e = Embedder::start(None, dir.path().to_path_buf(), true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
        while e.status() == EmbedderStatus::Downloading {
            assert!(
                std::time::Instant::now() < deadline,
                "init did not reach a terminal status within 10 minutes"
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        assert_eq!(e.status(), EmbedderStatus::Ready);
        let v = e.embed("hello embeddings").expect("Ready must embed");
        assert_eq!(v.len(), 384, "bge-small-en-v1.5 is 384-dim");
    }
}
