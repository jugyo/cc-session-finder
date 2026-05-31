//! Read-only MCP stdio server exposing indexed sessions. Tools are thin
//! wrappers over [`crate::sessions`]; stdout is reserved for JSON-RPC and all
//! diagnostics go to stderr via tracing.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{InitializeResult, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::index;
use crate::sessions::{
    self, MessageOrder, MessageSearchResponse, MessagesParams, MessagesResponse, OverviewResponse,
    SearchParams, SearchResponse,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(transform = crate::sessions::models::strip_int_formats)]
struct SearchSessionsInput {
    /// Search query. Omit or leave empty to list recent sessions.
    #[serde(default)]
    query: Option<String>,
    /// Max sessions to return (default 20, capped at 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Restrict to sessions from this working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// When true, restrict to the given cwd (or the server's cwd if none).
    #[serde(default)]
    cwd_only: Option<bool>,
    /// Lower mtime bound: a duration like "7d", a date, RFC3339, or Unix time.
    #[serde(default)]
    since: Option<String>,
    /// Upper mtime bound, same accepted forms as `since`.
    #[serde(default)]
    until: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetSessionOverviewInput {
    /// Opaque session id from a `search_sessions` result.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(transform = crate::sessions::models::strip_int_formats)]
struct GetSessionMessagesInput {
    /// Opaque session id.
    id: String,
    /// Max messages to return (default 10, capped at 30).
    #[serde(default)]
    limit: Option<usize>,
    /// "asc" (default) or "desc".
    #[serde(default)]
    order: Option<String>,
    /// Return messages with a greater message index.
    #[serde(default)]
    after_message_index: Option<u32>,
    /// Return messages with a smaller message index.
    #[serde(default)]
    before_message_index: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(transform = crate::sessions::models::strip_int_formats)]
struct SearchSessionMessagesInput {
    /// Opaque session id.
    id: String,
    /// Search query (required, non-empty).
    query: String,
    /// Max matches to return (default 10, capped at 30).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Clone)]
struct SessionsServer {
    tool_router: ToolRouter<Self>,
}

impl SessionsServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

/// Run `f` against a freshly opened cache DB connection on a blocking thread,
/// mapping any error to a tool error string.
async fn with_db<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let conn = index::open()?;
        f(&conn)
    })
    .await
    .map_err(|e| format!("task join error: {e}"))?
    .map_err(|e| format!("{e:#}"))
}

#[tool_router(router = tool_router)]
impl SessionsServer {
    #[tool(
        name = "search_sessions",
        description = "Search indexed Claude Code and Codex sessions by query, or list recent sessions when query is omitted. Use this first to find candidate sessions."
    )]
    async fn search_sessions(
        &self,
        Parameters(input): Parameters<SearchSessionsInput>,
    ) -> Result<Json<SearchResponse>, String> {
        let response = with_db(move |conn| {
            let time_range =
                crate::cli::parse_time_range(input.since.as_deref(), input.until.as_deref())?;
            let cwd_only = input.cwd_only.unwrap_or(false);
            let cwd = resolve_cwd(input.cwd.as_deref(), cwd_only);
            sessions::search_sessions(
                conn,
                SearchParams {
                    query: input.query,
                    limit: input.limit,
                    cwd,
                    cwd_only,
                    time_range,
                },
            )
        })
        .await?;
        Ok(Json(response))
    }

    #[tool(
        name = "get_session_overview",
        description = "Get a lightweight overview of one session: metadata, first and latest messages, and message count. Not a transcript retrieval tool."
    )]
    async fn get_session_overview(
        &self,
        Parameters(input): Parameters<GetSessionOverviewInput>,
    ) -> Result<Json<OverviewResponse>, String> {
        let id = input.id;
        let response = with_db(move |conn| sessions::get_session_overview(conn, &id)).await?;
        response
            .map(Json)
            .ok_or_else(|| "session not found".to_string())
    }

    #[tool(
        name = "get_session_messages",
        description = "Page through visible user/assistant messages of one session by message index."
    )]
    async fn get_session_messages(
        &self,
        Parameters(input): Parameters<GetSessionMessagesInput>,
    ) -> Result<Json<MessagesResponse>, String> {
        let response = with_db(move |conn| {
            let order = match input.order.as_deref() {
                Some("desc") => MessageOrder::Desc,
                _ => MessageOrder::Asc,
            };
            sessions::get_session_messages(
                conn,
                MessagesParams {
                    id: input.id,
                    limit: input.limit,
                    order,
                    after_message_index: input.after_message_index,
                    before_message_index: input.before_message_index,
                },
            )
        })
        .await?;
        response
            .map(Json)
            .ok_or_else(|| "session not found".to_string())
    }

    #[tool(
        name = "search_session_messages",
        description = "Search visible user/assistant messages within one session."
    )]
    async fn search_session_messages(
        &self,
        Parameters(input): Parameters<SearchSessionMessagesInput>,
    ) -> Result<Json<MessageSearchResponse>, String> {
        if input.query.trim().is_empty() {
            return Err("query must be non-empty".to_string());
        }
        let response = with_db(move |conn| {
            sessions::search_session_messages(conn, &input.id, &input.query, input.limit)
        })
        .await?;
        response
            .map(Json)
            .ok_or_else(|| "session not found".to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SessionsServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Read-only search over locally indexed Claude Code and Codex sessions. \
             Call search_sessions first to find candidate sessions, then \
             get_session_overview for a summary, and get_session_messages or \
             search_session_messages to read deeper. Session ids are opaque handles.",
            )
    }
}

fn resolve_cwd(cwd: Option<&str>, cwd_only: bool) -> Option<PathBuf> {
    match cwd {
        Some(p) => Some(PathBuf::from(p)),
        None if cwd_only => std::env::current_dir().ok(),
        None => None,
    }
}

/// Serve the MCP stdio server until the client disconnects.
pub fn run() -> Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = SessionsServer::new()
            .serve(rmcp::transport::stdio())
            .await?;
        service.waiting().await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(ExitCode::SUCCESS)
}
