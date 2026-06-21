//! Read-only MCP stdio server exposing indexed sessions. Tools are thin
//! wrappers over [`crate::sessions`]; stdout is reserved for JSON-RPC and all
//! diagnostics go to stderr via tracing.

use std::path::{Path, PathBuf};
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
    self, AutonomyParams, AutonomySessionsResponse, AutonomySort, InefficientParams,
    InefficientSessionsResponse, InefficientSort, MessageOrder, MessageSearchResponse,
    MessagesParams, MessagesResponse, OverviewResponse, SearchParams, SearchResponse,
    TrajectoryParams, TrajectoryResponse,
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
    /// Opaque continuation cursor returned by a previous search_sessions call.
    /// When set, the original query and filters are read from the cursor.
    #[serde(default)]
    cursor: Option<String>,
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

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(transform = crate::sessions::models::strip_int_formats)]
struct GetSessionTrajectoryInput {
    /// Opaque session id.
    id: String,
    /// Max steps to return (default 30, capped at 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Return steps with a greater step index (for paging).
    #[serde(default)]
    after_step_index: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(transform = crate::sessions::models::strip_int_formats)]
struct FindInefficientSessionsInput {
    /// Lower mtime bound: a duration like "7d", a date, RFC3339, or Unix time.
    #[serde(default)]
    since: Option<String>,
    /// Max sessions to return (default 20, capped at 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Ranking signal: "billable_tokens" (default), "error_rate", or
    /// "cache_read_ratio".
    #[serde(default)]
    sort_by: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(transform = crate::sessions::models::strip_int_formats)]
struct FindAutonomousSessionsInput {
    /// Lower mtime bound: a duration like "7d", a date, RFC3339, or Unix time.
    #[serde(default)]
    since: Option<String>,
    /// Max sessions to return (default 20, capped at 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Ranking signal: "max_run" (default), "mean_run", or "p90_run".
    #[serde(default)]
    sort_by: Option<String>,
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
    F: FnOnce(&mut rusqlite::Connection) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut conn = index::open()?;
        f(&mut conn)
    })
    .await
    .map_err(|e| format!("task join error: {e}"))?
    .map_err(|e| format!("{e:#}"))
}

#[tool_router(router = tool_router)]
impl SessionsServer {
    #[tool(
        name = "search_sessions",
        description = "Search indexed Claude Code and Codex sessions by query, or list recent sessions when query is omitted. Pass cursor from a previous response to fetch the next page."
    )]
    async fn search_sessions(
        &self,
        Parameters(input): Parameters<SearchSessionsInput>,
    ) -> Result<Json<SearchResponse>, String> {
        let response = with_db(move |conn| {
            index::ingest::scan_and_update(conn, false, &index::ingest::NoopProgress)?;
            let time_range =
                crate::cli::parse_time_range(input.since.as_deref(), input.until.as_deref())?;
            let cwd_only = input.cwd_only.unwrap_or(false);
            let cwd = resolve_cwd(input.cwd.as_deref(), cwd_only);
            sessions::search_sessions(
                conn,
                SearchParams {
                    query: input.query,
                    limit: input.limit,
                    cursor: input.cursor,
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
            index::ingest::scan_and_update(conn, false, &index::ingest::NoopProgress)?;
            sessions::search_session_messages(conn, &input.id, &input.query, input.limit)
        })
        .await?;
        response
            .map(Json)
            .ok_or_else(|| "session not found".to_string())
    }

    #[tool(
        name = "get_session_trajectory",
        description = "Page through the step-level trajectory of one session (one row per tool call, assistant turn, API error, or context-compaction event). Returns tool name and input, byte sizes, per-step token attribution, error and sidechain flags, MCP/skill attribution, stop reason, and duration. Use to drill into where a session spent tokens or hit errors."
    )]
    async fn get_session_trajectory(
        &self,
        Parameters(input): Parameters<GetSessionTrajectoryInput>,
    ) -> Result<Json<TrajectoryResponse>, String> {
        let response = with_db(move |conn| {
            sessions::get_session_trajectory(
                conn,
                TrajectoryParams {
                    id: input.id,
                    limit: input.limit,
                    after_step_index: input.after_step_index,
                },
            )
        })
        .await?;
        response
            .map(Json)
            .ok_or_else(|| "session not found".to_string())
    }

    #[tool(
        name = "find_inefficient_sessions",
        description = "Rank indexed sessions by an efficiency signal to surface outliers: sort_by billable_tokens (default), error_rate (tool errors / tool calls), or cache_read_ratio (cache reads / output tokens). Returns counts and ratios only, no message text."
    )]
    async fn find_inefficient_sessions(
        &self,
        Parameters(input): Parameters<FindInefficientSessionsInput>,
    ) -> Result<Json<InefficientSessionsResponse>, String> {
        let response = with_db(move |conn| {
            index::ingest::scan_and_update(conn, false, &index::ingest::NoopProgress)?;
            let time_range = crate::cli::parse_time_range(input.since.as_deref(), None)?;
            let sort_by = InefficientSort::parse(input.sort_by.as_deref())?;
            sessions::find_inefficient_sessions(
                conn,
                InefficientParams {
                    since: time_range.since,
                    limit: input.limit,
                    sort_by,
                },
            )
        })
        .await?;
        Ok(Json(response))
    }

    #[tool(
        name = "find_autonomous_sessions",
        description = "Rank indexed Claude sessions by autonomy: how long the agent runs uninterrupted between human turns. An autonomous run is a maximal stretch of trajectory steps with no human turn; sort_by max_run (default, longest run), mean_run, or p90_run. Each result includes run_count, total_steps, and tool_call_count as task-size indicators so autonomy can be compared within a size band. Counts only, no message text."
    )]
    async fn find_autonomous_sessions(
        &self,
        Parameters(input): Parameters<FindAutonomousSessionsInput>,
    ) -> Result<Json<AutonomySessionsResponse>, String> {
        let response = with_db(move |conn| {
            index::ingest::scan_and_update(conn, false, &index::ingest::NoopProgress)?;
            let time_range = crate::cli::parse_time_range(input.since.as_deref(), None)?;
            let sort_by = AutonomySort::parse(input.sort_by.as_deref())?;
            sessions::find_autonomous_sessions(
                conn,
                AutonomyParams {
                    since: time_range.since,
                    limit: input.limit,
                    sort_by,
                },
            )
        })
        .await?;
        Ok(Json(response))
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
             search_session_messages to read deeper. Use find_inefficient_sessions \
             to rank sessions by efficiency outliers (billable tokens, tool-error \
             rate, cache-read ratio), or find_autonomous_sessions to rank by how \
             long the agent runs uninterrupted between human turns (autonomy), then \
             get_session_trajectory to drill into the step-by-step tool calls, token \
             attribution, and errors of one session. Session ids are opaque handles.",
            )
    }
}

fn resolve_cwd(cwd: Option<&str>, cwd_only: bool) -> Option<PathBuf> {
    match cwd {
        Some(p) => Some(crate::paths::normalize_cwd_filter(Path::new(p))),
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
