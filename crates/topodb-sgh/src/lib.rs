//! sgh — Structured Graph Harness: frozen-DAG agent execution over TopoDB.
//!
//! Compile a goal into a validated graph (`schema`), compute its worst-case
//! bound before anything runs (`schema::bound`), execute it through pluggable
//! provider backends (`runner`, `provider`), and persist every state
//! transition in TopoDB (`store`).
pub mod executor;
pub mod planner;
pub mod provider;
pub mod recovery;
pub mod replan;
pub mod runner;
pub mod schema;
pub mod store;
