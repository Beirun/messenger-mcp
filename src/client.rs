use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use messenger_client::cookies::Jar as CrateJar;
use messenger_client::gateway::{Event, Gateway, Socket};
use messenger_client::{graphql, ls, ops, page};
use reqwest::Client as HttpClient;
use serde_json::Value;
use tokio::time::Instant as TokioInstant;

use crate::auth::CookieJar;

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36";
const MESSENGER_HOME: &str = "https://www.messenger.com/";

#[derive(Debug, PartialEq, Eq)]
pub enum SessionState {
    Active,
    LoginRequired,
    Interstitial,
    Unknown,
}

impl SessionState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::LoginRequired => "login required",
            Self::Interstitial => "request blocked",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Tokens {
    pub fb_dtsg: String,
    pub lsd: String,
}

#[derive(Debug)]
pub struct SessionReport {
    pub status: u16,
    pub bytes: usize,
    pub state: SessionState,
    pub tokens: Option<Tokens>,
}

pub struct Client {
    http: HttpClient,
    jar: CookieJar,
}

impl Client {
    pub fn new(jar: CookieJar) -> Result<Self> {
        let http = HttpClient::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building the HTTP client")?;
        Ok(Self { http, jar })
    }

    pub async fn check_session(&self) -> Result<SessionReport> {
        let (status, url, body) = self.home_page().await?;
        Ok(SessionReport {
            status,
            bytes: body.len(),
            state: classify(&url, &body),
            tokens: extract_tokens(&body),
        })
    }

    async fn home_page(&self) -> Result<(u16, String, String)> {
        let response = self
            .http
            .get(MESSENGER_HOME)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .header(
                "sec-ch-ua",
                "\"Chromium\";v=\"157\", \"Not_A Brand\";v=\"24\"",
            )
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Linux\"")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1")
            .header("Upgrade-Insecure-Requests", "1")
            .header("Cookie", self.jar.header_for("www.messenger.com"))
            .send()
            .await
            .context("requesting messenger.com")?;
        let status = response.status().as_u16();
        let url = response.url().to_string();
        let body = response.text().await.context("reading messenger.com")?;
        Ok((status, url, body))
    }

    pub async fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let (_, _, html) = self.home_page().await?;
        let config =
            page::scrape(&html).map_err(|err| anyhow::anyhow!("reading the page: {err}"))?;

        let mut all = Vec::new();
        for preloader in &config.preloaders {
            if !matches!(preloader.database, 1 | 95) {
                continue;
            }
            let steps = self.snapshot(&config, preloader).await?;
            let sync_group = u8::try_from(preloader.database).unwrap_or(1);
            all.extend(
                ls::calls(&steps)
                    .into_iter()
                    .map(|op| (op.to_vec(), sync_group)),
            );
        }

        let mut contacts = HashMap::new();
        let mut peers = HashMap::new();
        for (op, _) in &all {
            if let Some(participant) = ops::participant(op)
                && let Some(name) = participant.name
            {
                contacts.insert(participant.fb_user_id, name);
            }
            if let Some((thread, peer)) = hybrid_peer(op) {
                peers.insert(thread.to_owned(), peer.to_owned());
            }
        }

        let mut threads = Vec::new();
        for (op, sync_group) in &all {
            if let Some(thread) = ops::thread(op, *sync_group) {
                // A direct thread carries no title; its other participant names it.
                let title = thread.title.or_else(|| {
                    peers
                        .get(&thread.thread_key)
                        .and_then(|peer| contacts.get(peer))
                        .cloned()
                });
                threads.push(ThreadSummary {
                    thread_key: thread.thread_key,
                    title,
                    is_group: thread.is_group,
                    last_activity_ms: thread.last_activity_ms,
                    sync_group: *sync_group,
                });
            }
        }

        threads.sort_by_key(|thread| std::cmp::Reverse(thread.last_activity_ms.unwrap_or(0)));
        let mut seen = HashSet::new();
        threads.retain(|thread| seen.insert(thread.thread_key.clone()));
        Ok(threads)
    }

    async fn snapshot(
        &self,
        config: &page::PageConfig,
        preloader: &page::Preloader,
    ) -> Result<Value> {
        let form = graphql::snapshot_form(config, preloader);
        let response = self
            .http
            .post("https://www.messenger.com/api/graphql/")
            .header("Accept", "*/*")
            .header("Origin", MESSENGER_HOME)
            .header("Referer", MESSENGER_HOME)
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-origin")
            .header("X-Asbd-Id", "359341")
            .header(
                "X-Fb-Friendly-Name",
                "LSPlatformGraphQLLightspeedRequestQuery",
            )
            .header("X-Fb-Lsd", config.lsd.clone().unwrap_or_default())
            .header("Cookie", self.jar.header_for("www.messenger.com"))
            .form(&form)
            .send()
            .await
            .context("requesting the thread snapshot")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("reading the thread snapshot")?;
        if !status.is_success() {
            let preview: String = body.chars().take(200).collect();
            bail!("snapshot returned HTTP {status}: {preview}");
        }
        let payload =
            graphql::snapshot_payload(&body).map_err(|err| anyhow::anyhow!("snapshot: {err}"))?;
        let data: Value = serde_json::from_str(&payload).context("parsing the snapshot")?;
        Ok(data.get("step").cloned().unwrap_or(Value::Null))
    }

    pub async fn read_messages(
        &self,
        thread_key: &str,
        limit: usize,
        sync_group: u8,
    ) -> Result<Vec<MessageSummary>> {
        let raw = std::fs::read_to_string(crate::auth::cookies_path()?)
            .context("reading the cookie file")?;
        let mut jar = CrateJar::from_json(&raw).context("parsing the cookie file")?;

        let (_, _, html) = self.home_page().await?;
        let config =
            page::scrape(&html).map_err(|err| anyhow::anyhow!("reading the page: {err}"))?;

        let mut cursor = None;
        for preloader in &config.preloaders {
            if preloader.database != u32::from(sync_group) {
                continue;
            }
            let steps = self.snapshot(&config, preloader).await?;
            cursor = ls::sync_cursor(&steps);
            if cursor.is_some() {
                break;
            }
        }
        let cursor =
            cursor.with_context(|| format!("no sync cursor for sync group {sync_group}"))?;

        let mut socket = Socket::connect(&config, &mut jar)
            .await
            .context("connecting to the gateway")?;
        socket
            .open_sync(cursor.database, &cursor.next)
            .await
            .context("opening the sync stream")?;

        let deadline = TokioInstant::now() + Duration::from_secs(10);
        while let Some(event) = socket.next_event(deadline).await? {
            if let Event::Sync { database, .. } = event {
                if database == cursor.database {
                    break;
                }
            }
        }

        let key: i64 = thread_key.parse().context("thread key must be numeric")?;
        let payload = ls::fetch_messages(key, Utc::now().timestamp_millis(), sync_group);
        socket
            .send_task(ls::FETCH_MESSAGES, &format!("mrq.{key}"), payload)
            .await
            .context("sending the message request")?;

        let mut messages: Vec<MessageSummary> = Vec::new();
        let deadline = TokioInstant::now() + Duration::from_secs(10);
        while let Some(event) = socket.next_event(deadline).await? {
            match event {
                Event::Sync { database, steps } if database == cursor.database => {
                    for op in ls::calls(&steps) {
                        if let Some(message) = ops::message(op, Some(thread_key), sync_group) {
                            if message.thread_key == thread_key {
                                messages.push(MessageSummary {
                                    message_id: message.message_id,
                                    ts_ms: message.ts_ms,
                                    sender_id: message.sender_id,
                                    text: message.text,
                                });
                            }
                        }
                    }
                }
                Event::TaskDone if !messages.is_empty() => break,
                _ => {}
            }
        }
        socket.close().await;

        messages.sort_by_key(|message| std::cmp::Reverse(message.ts_ms));
        let mut seen = HashSet::new();
        messages.retain(|message| seen.insert(message.message_id.clone()));
        messages.truncate(limit);
        Ok(messages)
    }

    pub async fn send_text(
        &self,
        thread_key: &str,
        text: &str,
        sync_group: u8,
    ) -> Result<Option<String>> {
        let raw = std::fs::read_to_string(crate::auth::cookies_path()?)
            .context("reading the cookie file")?;
        let mut jar = CrateJar::from_json(&raw).context("parsing the cookie file")?;

        let (_, _, html) = self.home_page().await?;
        let config =
            page::scrape(&html).map_err(|err| anyhow::anyhow!("reading the page: {err}"))?;

        let mut cursor = None;
        for preloader in &config.preloaders {
            if preloader.database != u32::from(sync_group) {
                continue;
            }
            let steps = self.snapshot(&config, preloader).await?;
            cursor = ls::sync_cursor(&steps);
            if cursor.is_some() {
                break;
            }
        }
        let cursor =
            cursor.with_context(|| format!("no sync cursor for sync group {sync_group}"))?;

        let key: i64 = thread_key.parse().context("thread key must be numeric")?;
        let otid = Utc::now().timestamp_micros();
        let has_links = text.contains("http://") || text.contains("https://");
        let payload = serde_json::json!({
            "thread_id": key,
            "otid": otid.to_string(),
            "source": 65537,
            "send_type": 1,
            "sync_group": sync_group,
            "text": text,
            "initiating_source": 1,
            "skip_url_preview_gen": 0,
            "text_has_links": i32::from(has_links),
            "multitab_env": 0,
        });

        let mut socket = Socket::connect(&config, &mut jar)
            .await
            .context("connecting to the gateway")?;
        socket
            .open_sync(cursor.database, &cursor.next)
            .await
            .context("opening the sync stream")?;

        let deadline = TokioInstant::now() + Duration::from_secs(10);
        while let Some(event) = socket.next_event(deadline).await? {
            if let Event::Sync { database, .. } = event {
                if database == cursor.database {
                    break;
                }
            }
        }

        socket
            .send_task("46", &key.to_string(), payload)
            .await
            .context("sending the message task")?;

        // The send task's own answer is not the message; read the thread back instead.
        let ack = TokioInstant::now() + Duration::from_secs(5);
        while let Some(event) = socket.next_event(ack).await? {
            if matches!(event, Event::TaskDone) {
                break;
            }
        }

        let me = self.jar.user_id();
        let sent_after = otid / 1000 - 5_000;
        socket
            .send_task(
                ls::FETCH_MESSAGES,
                &format!("mrq.{key}"),
                ls::fetch_messages(key, Utc::now().timestamp_millis(), sync_group),
            )
            .await
            .context("requesting the thread")?;

        let mut sent_id = None;
        let deadline = TokioInstant::now() + Duration::from_secs(10);
        while let Some(event) = socket.next_event(deadline).await? {
            match event {
                Event::Sync { database, steps } if database == cursor.database => {
                    for op in ls::calls(&steps) {
                        if let Some(message) = ops::message(op, Some(thread_key), sync_group)
                            && message.thread_key == thread_key
                            && message.text.as_deref() == Some(text)
                            && message.sender_id.as_deref() == me
                            && message.ts_ms >= sent_after
                        {
                            sent_id = Some(message.message_id);
                        }
                    }
                }
                Event::TaskDone if sent_id.is_some() => break,
                _ => {}
            }
        }
        socket.close().await;
        Ok(sent_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub message_id: String,
    pub ts_ms: i64,
    pub sender_id: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_key: String,
    pub title: Option<String>,
    pub is_group: bool,
    pub last_activity_ms: Option<i64>,
    pub sync_group: u8,
}

fn boxed_string(value: &Value) -> Option<&str> {
    match value {
        Value::Array(items) if items.len() == 2 && items[0].as_u64() == Some(19) => {
            items[1].as_str()
        }
        _ => None,
    }
}

/// `verifyHybridThreadExists`: the encrypted thread key and its other participant.
fn hybrid_peer(op: &[Value]) -> Option<(&str, &str)> {
    if ops::name(op)? != "verifyHybridThreadExists" {
        return None;
    }
    let thread = boxed_string(op.get(2)?)?;
    let peer = boxed_string(op.get(3)?)?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (numeric(thread) && numeric(peer)).then_some((thread, peer))
}

fn extract_tokens(html: &str) -> Option<Tokens> {
    Some(Tokens {
        fb_dtsg: token_after(html, "DTSGInitialData")?,
        lsd: token_after(html, "[\"LSD\",[],{")?,
    })
}

fn token_after(html: &str, marker: &str) -> Option<String> {
    const KEY: &str = "\"token\":\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let start = rest.find(KEY)? + KEY.len();
    let rest = &rest[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn classify(url: &str, body: &str) -> SessionState {
    const INTERSTITIAL_MAX_BYTES: usize = 20_000;
    const ACTIVE_MIN_BYTES: usize = 100_000;
    if url.contains("/login") || body.contains("name=\"pass\"") {
        SessionState::LoginRequired
    } else if body.contains("Inbox") && body.len() >= ACTIVE_MIN_BYTES {
        SessionState::Active
    } else if body.len() <= INTERSTITIAL_MAX_BYTES
        && (body.contains("Your Request Couldn&#039;t be Processed")
            || body.contains("Your Request Couldn't be Processed"))
    {
        SessionState::Interstitial
    } else {
        SessionState::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_logged_in_shell() {
        let mut body = "x".repeat(200_000);
        body.push_str("Inbox");
        assert_eq!(
            classify("https://www.messenger.com/", &body),
            SessionState::Active
        );
    }

    #[test]
    fn detects_a_login_redirect() {
        assert_eq!(
            classify("https://www.messenger.com/login/", ""),
            SessionState::LoginRequired
        );
        assert_eq!(
            classify("https://www.messenger.com/", "<input name=\"pass\">"),
            SessionState::LoginRequired
        );
    }

    #[test]
    fn detects_the_interstitial() {
        assert_eq!(
            classify(
                "https://www.messenger.com/",
                "<h2>Your Request Couldn&#039;t be Processed</h2>"
            ),
            SessionState::Interstitial
        );
    }

    #[test]
    fn extracts_tokens_from_the_page() {
        let html = r#"["DTSGInitialData",[],{"token":"abc123"},258],["LSD",[],{"token":"lsd456"}]"#;
        assert_eq!(
            extract_tokens(html),
            Some(Tokens {
                fb_dtsg: "abc123".to_string(),
                lsd: "lsd456".to_string(),
            })
        );
    }

    #[test]
    fn reads_the_encrypted_thread_peer() {
        let op: Vec<Value> = serde_json::from_str(
            r#"[5,"verifyHybridThreadExists",[19,"5986541808137198"],[19,"100010550196790"],[19,"15"],[2,1],[9],false]"#,
        )
        .unwrap();
        assert_eq!(
            hybrid_peer(&op),
            Some(("5986541808137198", "100010550196790"))
        );
    }

    #[test]
    fn ignores_other_ops_and_malformed_peers() {
        let group: Vec<Value> =
            serde_json::from_str(r#"[5,"updateOrInsertThread",[19,"1"],[19,"2"]]"#).unwrap();
        assert_eq!(hybrid_peer(&group), None);
        let malformed: Vec<Value> =
            serde_json::from_str(r#"[5,"verifyHybridThreadExists",[19,"x"],[19,"2"]]"#).unwrap();
        assert_eq!(hybrid_peer(&malformed), None);
        let short: Vec<Value> = serde_json::from_str(r#"[5,"verifyHybridThreadExists"]"#).unwrap();
        assert_eq!(hybrid_peer(&short), None);
    }

    #[test]
    fn reports_missing_tokens() {
        assert_eq!(extract_tokens("<html></html>"), None);
    }
}
