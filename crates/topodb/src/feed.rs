//! Change feed: the sanctioned mechanism for observing committed writes.
//!
//! A [`ChangeEvent`] pairs a committed op with its monotonic op-log sequence
//! number. The feed has two surfaces, both on [`crate::db::Db`]:
//!
//! - `subscribe` — a live, best-effort push channel. Never blocks the
//!   applier: if a subscriber falls behind, events are *dropped* for it (the
//!   subscriber notices the `seq` gap and re-syncs).
//! - `ops_since` — a pull-based replay of the durable op log from a given
//!   `seq`, used to recover the ops that `subscribe` dropped.
//!
//! Both are **unscoped** host-level primitives, by spec design: the change
//! feed powers external consolidation/decay, which must see every write.

use crate::op::Op;

/// A single committed op paired with its op-log sequence number.
///
/// `seq` is the position in the durable op log (1-based, contiguous,
/// monotonically increasing). It is stable across restarts and identical
/// whether the event arrived via `subscribe` or `ops_since`, so subscribers
/// can detect gaps (a jump in `seq`) and recover the missing range with
/// `ops_since`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeEvent {
    pub seq: u64,
    pub op: Op,
}
