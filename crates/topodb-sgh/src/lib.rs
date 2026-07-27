//! sgh — Structured Graph Harness: frozen-DAG agent execution over TopoDB.
//!
//! Compile a goal into a validated graph (`schema`), compute its worst-case
//! bound before anything runs (`schema::bound`), execute it through pluggable
//! provider backends (`runner`, `provider`), and persist every state
//! transition in TopoDB (`store`).
//!
//! Worked example (CLI usage; the `sgh` binary lives in `src/bin/sgh.rs`):
//!
//! ```text
//! sgh plan "goal" --provider anthropic
//! sgh run .sgh/graph.yaml --provider anthropic \
//!     --agent-mcp '/abs/topodb-mcp --db /path/mem.redb --scope shared --embeddings off'
//! ```
pub mod events;
pub mod executor;
pub mod mcp_bridge;
pub mod planner;
pub mod provider;
pub mod recovery;
pub mod replan;
pub mod runner;
pub mod schema;
pub mod store;
