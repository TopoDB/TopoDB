//! Access counters: auxiliary hit/recency statistics tracked *outside* the op
//! log. Counters are deliberately not derived from the durable op log — they
//! are best-effort telemetry a host can use to drive recency/decay heuristics,
//! not part of the graph's authoritative state. As a consequence:
//!
//! - a bump is fire-and-forget (a read must never block or fail on a full/closed
//!   counter channel),
//! - counter mutations never appear in the change feed,
//! - `rebuild_state_from_ops` leaves the COUNTERS table untouched, and
//! - a `RemoveNode` may leave an orphan counter row behind (benign — reads of
//!   stats gate on node existence via the scoped snapshot check in
//!   `access_stats`, which deliberately does not bump).

use serde::{Deserialize, Serialize};

/// Per-node access statistics: how many times a node has been returned by a
/// scoped read, and the wall-clock millisecond timestamp of the most recent
/// such read. `Default` (both zero) means "exists but never counted".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccessStats {
    pub access_count: u64,
    pub last_accessed_at: i64,
}
