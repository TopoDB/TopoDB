#![doc = include_str!("../README.md")]

#[allow(dead_code)] // Task 4 codec is wired into dual writes in Task 5.
mod adj;
mod codec;
mod counters;
mod db;
mod dict;
mod disk;
mod error;
mod feed;
mod fts;
mod graph;
mod ids;
mod index;
mod migrate;
#[allow(dead_code)] // v3 cutover is staged; migration helpers land before open_with flips.
mod migrate_v3;
mod op;
#[allow(dead_code)] // dual-maintained until the v3 disk read cutover.
mod prop_index;
mod props;
mod read;
#[allow(dead_code)] // Scope IDs are consumed by v3 re-keyed rows.
mod scopes;
#[allow(dead_code)] // allocated tables are wired incrementally in v3 Tasks 2–7.
mod slots;
mod state;
mod storage;
mod validate;
mod vector;

pub use counters::AccessStats;
pub use db::Db;
pub use error::TopoError;
pub use feed::ChangeEvent;
#[doc(hidden)]
pub use graph::AdjEntry;
pub use graph::Snapshot;
pub use ids::{EdgeId, NodeId, Scope, ScopeId, ScopeSet};
pub use index::{IndexSpec, PropIndex};
pub use op::Op;
pub use props::{PropValue, Props};
pub use read::{Direction, Subgraph, TraversalQuery};
pub use state::{EdgeRecord, NodeRecord};
pub use storage::AppliedBatch;
#[doc(hidden)]
pub use storage::TableReport;
pub use vector::VectorQuery;
#[doc(hidden)]
pub mod workload;
