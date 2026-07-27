//! Sidecar run events: a best-effort JSONL projection of the run. The db is
//! the durable truth; this stream exists so `sgh show` (and anything else)
//! can watch a live run without touching the exclusively-locked db.
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        run_id: String,
        goal: String,
        agent_calls_bound: u64,
        command_runs_bound: u64,
    },
    NodeStarted {
        node_id: String,
    },
    AttemptFinished {
        node_id: String,
        rung: String,
        error: String,
    },
    NodeSucceeded {
        node_id: String,
    },
    NodeBlocked {
        node_id: String,
        reason: Option<String>,
    },
    NodeSkipped {
        node_id: String,
    },
    GateReached {
        node_id: String,
    },
    RunFinished {
        succeeded: Vec<String>,
        blocked: Vec<String>,
        skipped: Vec<String>,
        model_calls: u64,
        command_runs: u64,
    },
}

/// One JSONL line: {"v":1,"ts":<ms>,"event":"node_started",...}
#[derive(Serialize)]
struct Envelope<'a> {
    v: u32,
    ts: i64,
    #[serde(flatten)]
    ev: &'a RunEvent,
}

pub trait EventSink: Send + Sync {
    /// MUST NOT panic and MUST NOT block meaningfully; errors are the sink's
    /// problem (warn-once + self-disable), never the run's.
    fn emit(&self, ts: i64, ev: &RunEvent);
}

/// Best-effort JSONL projection of a run onto a file. Any I/O error
/// (including a poisoned mutex, which is treated the same as a broken
/// writer) disables the sink permanently after a single `eprintln!` — a sink
/// that cannot write must never be allowed to panic or otherwise interrupt
/// the run it is merely observing.
pub struct JsonlSink {
    writer: Mutex<BufWriter<File>>,
    disabled: AtomicBool,
}

impl JsonlSink {
    /// Creates parent dirs; opens in append mode.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(JsonlSink {
            writer: Mutex::new(BufWriter::new(file)),
            disabled: AtomicBool::new(false),
        })
    }

    fn disable(&self, err: impl std::fmt::Display) {
        eprintln!("sgh: event log disabled: {err}");
        self.disabled.store(true, Ordering::SeqCst);
    }
}

impl EventSink for JsonlSink {
    fn emit(&self, ts: i64, ev: &RunEvent) {
        if self.disabled.load(Ordering::SeqCst) {
            return;
        }

        let envelope = Envelope { v: 1, ts, ev };
        let line = match serde_json::to_string(&envelope) {
            Ok(l) => l,
            Err(e) => {
                self.disable(e);
                return;
            }
        };

        let mut guard = match self.writer.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                // A poisoned mutex means some other emit panicked mid-write
                // (should not happen — nothing here panics — but treat it as
                // a dead sink rather than propagating the poison).
                self.disable("writer mutex poisoned");
                let _ = poisoned;
                return;
            }
        };

        let write_result = writeln!(guard, "{line}").and_then(|_| guard.flush());
        if let Err(e) = write_result {
            drop(guard);
            self.disable(e);
        }
    }
}

/// Test/support sink capturing events in memory.
pub struct VecSink(pub Mutex<Vec<(i64, RunEvent)>>);

impl VecSink {
    pub fn new() -> Self {
        VecSink(Mutex::new(Vec::new()))
    }
}

impl Default for VecSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for VecSink {
    fn emit(&self, ts: i64, ev: &RunEvent) {
        self.0.lock().unwrap().push((ts, ev.clone()));
    }
}
