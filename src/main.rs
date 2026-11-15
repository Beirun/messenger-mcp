use std::path::PathBuf;

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use tracing_subscriber::EnvFilter;

mod auth;
mod login;

use auth::CookieJar;

#[derive(Clone)]
struct Messenger {
    #[allow(dead_code)]
    tool_router: ToolRouter<Messenger>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadMessagesArgs {
    /// Chat or thread id
    chat_id: String,
    /// Maximum number of messages to return
    limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SendMessageArgs {
    /// Chat or thread id
    chat_id: String,
    /// Message text
    text: String,
}

#[tool_router]
impl Messenger {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Report login and configuration state")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let Some(path) = std::env::var_os("MESSENGER_COOKIES") else {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "no cookie file configured; set MESSENGER_COOKIES",
            )]));
        };
        let path = PathBuf::from(path);
        let jar = match CookieJar::load(&path) {
            Ok(jar) => jar,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not load cookies from {}: {err:#}",
                    path.display()
                ))]));
            }
        };
        let mut lines = vec![
            format!("cookie file: {}", path.display()),
            format!("cookies loaded: {}", jar.len()),
            format!("user id: {}", jar.user_id().unwrap_or("unknown")),
        ];
        let missing = jar.missing_required();
        if missing.is_empty() {
            lines.push("required cookies: c_user, xs present".to_string());
        } else {
            lines.push(format!("required cookies missing: {}", missing.join(", ")));
        }
        lines.push("session: not verified; network checks are not implemented yet".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "List recent chats")]
    async fn list_chats(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(
            "list_chats is not implemented yet",
        )]))
    }

    #[tool(description = "Read messages from a chat")]
    async fn read_messages(
        &self,
        Parameters(ReadMessagesArgs { chat_id, limit }): Parameters<ReadMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "read_messages is not implemented yet (chat_id={chat_id}, limit={})",
            limit.unwrap_or(20)
        ))]))
    }

    #[tool(description = "Send a text message to a chat")]
    async fn send_message(
        &self,
        Parameters(SendMessageArgs { chat_id, text }): Parameters<SendMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "send_message is not implemented yet (chat_id={chat_id}, {} chars)",
            text.chars().count()
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for Messenger {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "messenger-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Messenger MCP: read and send Facebook Messenger messages. \
                 Personal-account client, unofficial; not implemented yet."
                    .to_string(),
            )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("login") => return login::run().await,
        Some(other) => anyhow::bail!("unknown command {other}; expected no arguments or \"login\""),
        None => {}
    }

    let service = Messenger::new().serve(stdio()).await.inspect_err(|e| {
        tracing::error!("serving error: {:?}", e);
    })?;
    service.waiting().await?;
    Ok(())
}
