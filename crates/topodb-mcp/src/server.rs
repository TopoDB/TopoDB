//! The rmcp server handler wrapping a TopoDB [`Db`].
//!
//! Built on rmcp 2.2.0: the tool surface is declared with `#[tool_router]` +
//! `#[tool]` and dispatched through `#[tool_handler]` on the [`ServerHandler`]
//! impl. Tasks 4-5 add the read/write tools to the same `#[tool_router]` block,
//! following the `db_info` pattern established here.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::Serialize;
use topodb::{Db, Scope, ScopeSet};

use crate::config::{scope_label, Config};

/// The MCP server state. `Clone` is required by rmcp (the service clones the
/// handler per request); every field is cheap to clone — [`Db`] is an `Arc`
/// handle, [`ScopeSet`] is a small set, and the rest are owned metadata.
#[derive(Clone)]
pub struct TopoServer {
    db: Db,
    /// The configured default scope, applied to tool calls that omit `scope`.
    default_scope: Scope,
    /// The default scope pre-resolved to a [`ScopeSet`] for the scoped read
    /// tools that Tasks 4-5 add. Held here so every call reuses one set.
    #[allow(dead_code)]
    default_scopes: ScopeSet,
    /// Rendered db path, reported by `db_info`.
    db_path: String,
    tool_router: ToolRouter<Self>,
}

impl TopoServer {
    /// Wraps an open [`Db`] and the resolved [`Config`] into a server handler.
    pub fn new(db: Db, config: &Config) -> Self {
        let default_scopes = match config.default_scope {
            Scope::Shared => ScopeSet::default().with_shared(),
            Scope::Id(id) => ScopeSet::of(&[id]),
        };
        Self {
            db,
            default_scope: config.default_scope,
            default_scopes,
            db_path: config.db_path.display().to_string(),
            tool_router: Self::tool_router(),
        }
    }
}

/// The `db_info` result payload. `Json<DbInfo>` (below) makes it structured
/// tool output.
#[derive(Debug, Serialize, JsonSchema)]
struct DbInfo {
    /// Filesystem path of the open database.
    path: String,
    /// Highest op-log sequence number committed so far (0 on a fresh db). Use
    /// this as the `since_seq` anchor for `get_changes`.
    current_seq: u64,
    /// Default scope applied to tool calls that omit `scope`: `"shared"` or a
    /// ULID string.
    default_scope: String,
}

#[tool_router]
impl TopoServer {
    #[tool(
        description = "Report the open database's path, current op-log sequence number, and the default scope applied to tool calls that omit `scope`. Call this first to confirm the server is wired to the expected database, and to obtain current_seq as the anchor for get_changes."
    )]
    fn db_info(&self) -> Result<Json<DbInfo>, ErrorData> {
        let current_seq = self
            .db
            .current_seq()
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(Json(DbInfo {
            path: self.db_path.clone(),
            current_seq,
            default_scope: scope_label(&self.default_scope),
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TopoServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo::new` defaults `server_info` to rmcp's own
        // `Implementation::from_build_env()` (reporting "rmcp"/its version), so
        // override it with this crate's identity.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "TopoDB agent-memory engine exposed over MCP: a temporal property graph with \
                 scoped recall. Tool calls that omit `scope` use the server's configured default \
                 scope (see db_info). Start with db_info to confirm wiring.",
            )
    }
}
