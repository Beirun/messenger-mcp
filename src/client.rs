use anyhow::{Context, Result};
use reqwest::Client as HttpClient;

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
        Ok(SessionReport {
            status,
            bytes: body.len(),
            state: classify(&url, &body),
            tokens: extract_tokens(&body),
        })
    }
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
