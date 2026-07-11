#![doc = include_str!("../README.md")]

mod counters;
mod db;
mod error;
mod feed;
mod fts;
mod graph;
mod ids;
mod index;
mod op;
mod props;
mod read;
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
pub use vector::VectorQuery;
