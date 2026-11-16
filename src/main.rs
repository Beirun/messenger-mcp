use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use tracing_subscriber::EnvFilter;

use messenger_cli::{auth, browser, client, login};

use auth::CookieJar;
use client::Client;

#[derive(Clone)]
struct Messenger {
    #[allow(dead_code)]
    tool_router: ToolRouter<Messenger>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadMessagesArgs {
    /// Chat or thread id
    chat_id: String,
    /// Maximum number of messages to return (1 to 100)
    limit: Option<u32>,
    /// Use browser for encrypted conversations, gateway for supported conversations
    backend: Option<ReadBackend>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ReadBackend {
    Gateway,
    Browser,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SendMessageArgs {
    /// Chat or thread id
    chat_id: String,
    /// Message text
    text: String,
    /// Use browser for encrypted conversations, gateway for supported conversations
    backend: Option<ReadBackend>,
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
        let path = match auth::cookies_path() {
            Ok(path) => path,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "cannot resolve the cookie path: {err:#}"
                ))]));
            }
        };
        if !path.exists() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "no cookie file at {}; run `messenger-mcp login`",
                path.display()
            ))]));
        }
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
            match Client::new(jar) {
                Ok(client) => match client.check_session().await {
                    Ok(report) => {
                        lines.push(format!(
                            "session: {} (HTTP {}, {} KB)",
                            report.state.label(),
                            report.status,
                            report.bytes / 1024
                        ));
                        if report.state == client::SessionState::Active {
                            lines.push(if report.tokens.is_some() {
                                "api tokens: found".to_string()
                            } else {
                                "api tokens: missing".to_string()
                            });
                        }
                    }
                    Err(err) => lines.push(format!("session: check failed: {err:#}")),
                },
                Err(err) => lines.push(format!("session: check failed: {err:#}")),
            }
        } else {
            lines.push(format!("required cookies missing: {}", missing.join(", ")));
            lines.push("session: not checked".to_string());
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            lines.join("\n"),
        )]))
    }

    #[tool(description = "List recent chats")]
    async fn list_chats(&self) -> Result<CallToolResult, McpError> {
        let path = match auth::cookies_path() {
            Ok(path) => path,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "cannot resolve the cookie path: {err:#}"
                ))]));
            }
        };
        let jar = match CookieJar::load(&path) {
            Ok(jar) if jar.missing_required().is_empty() => jar,
            Ok(_) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "required cookies are missing; run `messenger-mcp login`",
                )]));
            }
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not load cookies from {}: {err:#}",
                    path.display()
                ))]));
            }
        };
        let client = match Client::new(jar) {
            Ok(client) => client,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not build the client: {err:#}"
                ))]));
            }
        };
        match client.list_threads().await {
            Ok(threads) => {
                let mut lines = vec![format!("{} chats", threads.len())];
                for thread in threads.iter().take(50) {
                    let kind = if thread.is_group { "group" } else { "direct" };
                    let title = thread
                        .title
                        .clone()
                        .unwrap_or_else(|| "(untitled)".to_string());
                    let when = thread
                        .last_activity_ms
                        .map(format_ms)
                        .unwrap_or_else(|| "-".to_string());
                    lines.push(format!(
                        "- {title} [{kind}] {when} key={}",
                        thread.thread_key
                    ));
                }
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    lines.join("\n"),
                )]))
            }
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "list_chats failed: {err:#}"
            ))])),
        }
    }

    #[tool(description = "Read messages from a chat")]
    async fn read_messages(
        &self,
        Parameters(ReadMessagesArgs {
            chat_id,
            limit,
            backend,
        }): Parameters<ReadMessagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        if chat_id.is_empty() || !chat_id.bytes().all(|b| b.is_ascii_digit()) || chat_id.len() > 19
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "chat_id must be a numeric thread ID",
            )]));
        }
        let limit = limit.unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "limit must be between 1 and 100",
            )]));
        }
        let path = match auth::cookies_path() {
            Ok(path) => path,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "cannot resolve the cookie path: {err:#}"
                ))]));
            }
        };
        let jar = match CookieJar::load(&path) {
            Ok(jar) if jar.missing_required().is_empty() => jar,
            Ok(_) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "required cookies are missing; run `messenger-mcp login`",
                )]));
            }
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not load cookies from {}: {err:#}",
                    path.display()
                ))]));
            }
        };
        if matches!(backend, Some(ReadBackend::Browser)) {
            return match browser::read_messages(&chat_id, limit as usize, &jar).await {
                Ok(messages) => {
                    let mut lines = vec![format!(
                        "{} loaded messages, newest first; history may be incomplete",
                        messages.len()
                    )];
                    for message in &messages {
                        let text: String = message.text.chars().take(500).collect();
                        lines.push(format!("- {text} (id {})", message.id));
                    }
                    Ok(CallToolResult::success(vec![ContentBlock::text(
                        lines.join("\n"),
                    )]))
                }
                Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "browser read failed: {err:#}"
                ))])),
            };
        }
        let client = match Client::new(jar) {
            Ok(client) => client,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not build the client: {err:#}"
                ))]));
            }
        };
        match client.read_messages(&chat_id, limit as usize, 1).await {
            Ok(messages) if messages.is_empty() => {
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "no messages returned for {chat_id}; it may be an end-to-end encrypted chat, which is not supported yet"
                ))]))
            }
            Ok(messages) => {
                let mut lines = vec![format!("{} messages", messages.len())];
                for message in &messages {
                    let when = format_ms(message.ts_ms);
                    let sender = message.sender_id.clone().unwrap_or_else(|| "?".to_string());
                    let text = message
                        .text
                        .clone()
                        .unwrap_or_else(|| "(no text)".to_string());
                    lines.push(format!("- {when} {sender}: {text}"));
                }
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    lines.join("\n"),
                )]))
            }
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "read_messages failed: {err:#}"
            ))])),
        }
    }

    #[tool(description = "Send a text message to a chat")]
    async fn send_message(
        &self,
        Parameters(SendMessageArgs {
            chat_id,
            text,
            backend,
        }): Parameters<SendMessageArgs>,
    ) -> Result<CallToolResult, McpError> {
        if chat_id.is_empty() || !chat_id.bytes().all(|b| b.is_ascii_digit()) || chat_id.len() > 19
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "chat_id must be a numeric thread ID",
            )]));
        }
        if text.trim().is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "text must not be empty",
            )]));
        }
        let path = match auth::cookies_path() {
            Ok(path) => path,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "cannot resolve the cookie path: {err:#}"
                ))]));
            }
        };
        let jar = match CookieJar::load(&path) {
            Ok(jar) if jar.missing_required().is_empty() => jar,
            Ok(_) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "required cookies are missing; run `messenger-mcp login`",
                )]));
            }
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not load cookies from {}: {err:#}",
                    path.display()
                ))]));
            }
        };
        if matches!(backend, Some(ReadBackend::Browser)) {
            return match browser::send_message(&chat_id, &text, &jar).await {
                Ok(message) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "sent to {chat_id} through the browser (message {})",
                    message.id
                ))])),
                Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "browser send failed: {err:#}"
                ))])),
            };
        }
        let client = match Client::new(jar) {
            Ok(client) => client,
            Err(err) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "could not build the client: {err:#}"
                ))]));
            }
        };
        match client.send_text(&chat_id, &text, 1).await {
            Ok(Some(id)) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "sent message {id} to {chat_id}"
            ))])),
            Ok(None) => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "sent to {chat_id}; reading the thread back did not confirm delivery. \
                 If this is an encrypted chat, send with backend=browser"
            ))])),
            Err(err) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "send_message failed: {err:#}"
            ))])),
        }
    }
}

fn format_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
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
