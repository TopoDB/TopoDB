//! On-demand, refcount-leased wrapper around `McpBridge`.
//!
//! Node-scoped bridge lifecycle: the `topodb-mcp` child — which holds
//! redb's exclusive lock on the memory db for its whole process lifetime —
//! is spawned when the first lease is taken and killed (kill+wait, via
//! `McpBridge::Drop`) when the last lease drops. Between tool-using nodes
//! the memory db is therefore unlocked and `command` nodes may read it.
//!
//! The MCP protocol use is stateless per call (initialize handshake +
//! tools/list cache + tools/call; no sessions or cursors), so the child can
//! be killed and respawned freely; a respawn costs process start + db open
//! + embedder start.
use super::{BridgeError, McpBridge};
use crate::provider::ToolDef;
use serde_json::Value;
use std::sync::Mutex;

#[derive(Debug, Default)]
struct Slot {
    bridge: Option<McpBridge>,
    leases: usize,
}

#[derive(Debug)]
pub struct OnDemandBridge {
    /// Pre-validated by rails::validate_mcp_server_command upstream.
    argv: Vec<String>,
    inner: Mutex<Slot>,
}

impl OnDemandBridge {
    /// No child is spawned here — construction is free, so the CLI can
    /// build this before the approval gate without starting a server for
    /// a run the operator rejects.
    pub fn new(argv: Vec<String>) -> Self {
        OnDemandBridge {
            argv,
            inner: Mutex::new(Slot::default()),
        }
    }

    /// Take a lease for one node execution. Spawns the child (handshake +
    /// tools/list) iff the slot is empty. On spawn failure the lease count
    /// is left untouched and the error is returned — the caller's node
    /// fails; a later lease may retry the spawn.
    pub fn lease(&self) -> Result<BridgeLease<'_>, BridgeError> {
        let mut slot = self
            .inner
            .lock()
            .map_err(|_| BridgeError::Malformed("mcp bridge mutex was poisoned".to_string()))?;
        if slot.bridge.is_none() {
            slot.bridge = Some(McpBridge::spawn(&self.argv)?);
        }
        slot.leases += 1;
        Ok(BridgeLease { owner: self })
    }
}

impl Drop for OnDemandBridge {
    fn drop(&mut self) {
        // get_mut bypasses locking, and matching the poisoned arm too means
        // the child is reaped on every exit path, including an unwinding
        // one that poisoned the slot.
        match self.inner.get_mut() {
            Ok(slot) => {
                slot.bridge.take(); // McpBridge::Drop = kill + wait
            }
            Err(poisoned) => {
                poisoned.into_inner().bridge.take();
            }
        }
    }
}

pub struct BridgeLease<'a> {
    owner: &'a OnDemandBridge,
}

impl BridgeLease<'_> {
    /// Snapshot of the child's tool list (namespaced `topodb__<name>`).
    /// Locks per operation so parallel nodes interleave between calls,
    /// exactly as the previous `Mutex<McpBridge>` did.
    pub fn tools(&self) -> Result<Vec<ToolDef>, BridgeError> {
        let slot = self
            .owner
            .inner
            .lock()
            .map_err(|_| BridgeError::Malformed("mcp bridge mutex was poisoned".to_string()))?;
        match &slot.bridge {
            Some(b) => Ok(b.tools().to_vec()),
            // Slot cleared by a dead-child error since this lease was taken;
            // the caller treats this like any other bridge failure.
            None => Err(BridgeError::ServerGone),
        }
    }

    /// Forward one tools/call. Holds the slot lock across the blocking
    /// call (one request in flight — the protocol layer requires it).
    /// Io/ServerGone/Malformed clear the slot so the NEXT lease (or next
    /// operation that respawns) gets a fresh child; `Tool` errors are a
    /// healthy child reporting a tool failure and never clear the slot.
    pub fn call(&self, name: &str, arguments: &Value) -> Result<String, BridgeError> {
        let mut slot = self
            .owner
            .inner
            .lock()
            .map_err(|_| BridgeError::Malformed("mcp bridge mutex was poisoned".to_string()))?;
        if slot.bridge.is_none() {
            // Dead child was cleared mid-lease; respawn on demand.
            slot.bridge = Some(McpBridge::spawn(&self.owner.argv)?);
        }
        let bridge = slot.bridge.as_mut().expect("filled just above");
        match bridge.call(name, arguments) {
            Ok(text) => Ok(text),
            Err(BridgeError::Tool(msg)) => Err(BridgeError::Tool(msg)),
            Err(e) => {
                slot.bridge.take(); // kill+wait the dead child now
                Err(e)
            }
        }
    }
}

impl Drop for BridgeLease<'_> {
    fn drop(&mut self) {
        // lock().ok(): a poisoned slot means the process is unwinding;
        // OnDemandBridge::Drop still reaps the child via get_mut.
        if let Ok(mut slot) = self.owner.inner.lock() {
            slot.leases = slot.leases.saturating_sub(1);
            if slot.leases == 0 {
                slot.bridge.take(); // McpBridge::Drop = kill + wait
            }
        }
    }
}
