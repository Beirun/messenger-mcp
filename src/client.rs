use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use messenger_client::{graphql, ls, ops, page};
use reqwest::Client as HttpClient;
use serde_json::Value;

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
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("sec-ch-ua", "\"Chromium\";v=\"157\", \"Not_A Brand\";v=\"24\"")
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
        let config = page::scrape(&html)
            .map_err(|err| anyhow::anyhow!("reading the page: {err}"))?;

        let mut threads = Vec::new();
        for preloader in &config.preloaders {
            if !matches!(preloader.database, 1 | 95) {
                continue;
            }
            let steps = self.snapshot(&config, preloader).await?;
            let sync_group = u8::try_from(preloader.database).unwrap_or(1);
            for op in ls::calls(&steps) {
                if let Some(thread) = ops::thread(op, sync_group) {
                    threads.push(ThreadSummary {
                        thread_key: thread.thread_key,
                        title: thread.title,
                        is_group: thread.is_group,
                        last_activity_ms: thread.last_activity_ms,
                        sync_group,
                    });
                }
            }
        }

        threads.sort_by_key(|thread| std::cmp::Reverse(thread.last_activity_ms.unwrap_or(0)));
        let mut seen = HashSet::new();
        threads.retain(|thread| seen.insert(thread.thread_key.clone()));
        Ok(threads)
    }

    async fn snapshot(&self, config: &page::PageConfig, preloader: &page::Preloader) -> Result<Value> {
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
            .header("X-Fb-Friendly-Name", "LSPlatformGraphQLLightspeedRequestQuery")
            .header("X-Fb-Lsd", config.lsd.clone().unwrap_or_default())
            .header("Cookie", self.jar.header_for("www.messenger.com"))
            .form(&form)
            .send()
            .await
            .context("requesting the thread snapshot")?;

        let status = response.status();
        let body = response.text().await.context("reading the thread snapshot")?;
        if !status.is_success() {
            let preview: String = body.chars().take(200).collect();
            bail!("snapshot returned HTTP {status}: {preview}");
        }
        let payload =
            graphql::snapshot_payload(&body).map_err(|err| anyhow::anyhow!("snapshot: {err}"))?;
        let data: Value = serde_json::from_str(&payload).context("parsing the snapshot")?;
        Ok(data.get("step").cloned().unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_key: String,
    pub title: Option<String>,
    pub is_group: bool,
    pub last_activity_ms: Option<i64>,
    pub sync_group: u8,
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
    fn reports_missing_tokens() {
        assert_eq!(extract_tokens("<html></html>"), None);
    }
}
