use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::{auth::CookieJar, login};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) fn lock_profile(profile: &Path) -> Result<File> {
    fs::create_dir_all(profile)?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(profile.join("messenger-mcp.lock"))?;
    lock.try_lock()
        .context("browser profile is in use by messenger-mcp")?;
    if fs::symlink_metadata(profile.join("SingletonLock")).is_ok() {
        bail!(
            "Chrome profile is in use or has a stale SingletonLock; close Chrome and check the profile"
        );
    }
    Ok(lock)
}

struct OwnedChrome {
    child: Child,
    profile: PathBuf,
}

impl Drop for OwnedChrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A killed Chrome leaves its singleton files behind; the profile lock is held,
        // so any left here belong to this process.
        for name in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
            let _ = fs::remove_file(self.profile.join(name));
        }
    }
}

struct Cdp {
    ws: Socket,
    id: u64,
}
impl Cdp {
    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            request["sessionId"] = json!(session);
        }
        self.ws
            .send(Message::Text(request.to_string().into()))
            .await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let message = timeout(
                deadline.saturating_duration_since(Instant::now()),
                self.ws.next(),
            )
            .await
            .context("browser request timed out")?
            .context("browser connection closed")??;
            if let Message::Text(text) = message {
                let response: Value = serde_json::from_str(&text)?;
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if response.get("error").is_some() {
                    bail!("browser rejected {method}");
                }
                return Ok(response.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BrowserMessage {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct PageState {
    path: String,
    state: String,
    messages: Vec<BrowserMessage>,
    #[serde(default)]
    list: bool,
}

const INSPECT: &str = r#"(() => {
 const path = location.pathname.replace(/\/$/, '');
 const text = document.body?.innerText || '';
 const dialog = [...document.querySelectorAll('[role=dialog]')].filter(e => e.getClientRects().length).map(e => e.innerText).join(' ');
 const list = document.querySelector('[data-pagelet="MWV2MessageList"], [aria-label^="Messages in conversation"]');
 const hasList = !!list;
 if (location.pathname.includes('login') || document.querySelector('input[type=password]')) return {path,state:'login',messages:[],list:hasList};
 if (/PIN to restore|one-time code|restore your chats/i.test(dialog)) return {path,state:'recovery',messages:[],list:hasList};
 if (/checkpoint|confirm your identity/i.test(location.href + ' ' + dialog)) return {path,state:'checkpoint',messages:[],list:hasList};
 if (/loading messages/i.test(list ? (list.innerText || '') : text)) return {path,state:'loading',messages:[],list:hasList};
 const nodes = [...document.querySelectorAll('[data-scope="messages_table"][data-message-id]')].filter(e => e.getClientRects().length);
 const messages = nodes.map(e => ({id:e.getAttribute('data-message-id'),text:(e.innerText || e.getAttribute('aria-label') || '').slice(0,4000)})).filter(m => m.id && m.text);
 if (messages.length) return {path,state:'ready',messages,list:hasList};
 return {path,state:'unknown',messages:[],list:hasList};
})()"#;

fn validate(state: &PageState, chat_id: &str) -> Result<()> {
    if state.path != format!("/e2ee/t/{chat_id}") {
        bail!("browser opened a different conversation or redirected");
    }
    match state.state.as_str() {
        "login" => bail!("Messenger requires sign-in; run messenger-mcp login"),
        "recovery" => bail!("Messenger requires encrypted history recovery in Chrome"),
        "checkpoint" => bail!("Messenger requires a security checkpoint in Chrome"),
        "loading" => bail!("timed out waiting for messages"),
        "ready" => Ok(()),
        _ => bail!("conversation layout was not recognized or messages did not load"),
    }
}

fn finish(state: PageState, chat_id: &str, limit: usize) -> Result<Vec<BrowserMessage>> {
    validate(&state, chat_id)?;
    let mut seen = std::collections::HashSet::new();
    let mut messages: Vec<_> = state
        .messages
        .into_iter()
        .rev()
        .filter(|m| seen.insert(m.id.clone()))
        .take(limit)
        .collect();
    messages.truncate(limit);
    Ok(messages)
}

struct Page {
    // Held for its Drop, which stops Chrome and clears the profile's singleton files.
    _chrome: OwnedChrome,
    cdp: Cdp,
    session: String,
    url: String,
}

impl Page {
    async fn open(chat_id: &str, jar: &CookieJar) -> Result<Self> {
        let profile = login::profile_dir()?;
        let port_file = profile.join("DevToolsActivePort");
        if port_file.exists() {
            fs::remove_file(&port_file).context("removing stale DevTools port")?;
        }
        let child = Command::new(login::find_chrome()?)
            .arg("--headless=new")
            .arg("--remote-debugging-address=127.0.0.1")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("launching Chrome")?;
        let mut chrome = OwnedChrome {
            child,
            profile: profile.clone(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let (port, path) = loop {
            if let Some(status) = chrome.child.try_wait()? {
                bail!("Chrome exited before opening DevTools ({status})");
            }
            if let Ok(value) = fs::read_to_string(&port_file) {
                let mut lines = value.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    if let Ok(port) = port.parse::<u16>() {
                        break (port, path.to_owned());
                    }
                }
            }
            if Instant::now() >= deadline {
                bail!("Chrome did not open DevTools");
            }
            sleep(Duration::from_millis(100)).await;
        };
        let (ws, _) = timeout(
            Duration::from_secs(10),
            connect_async(format!("ws://127.0.0.1:{port}{path}")),
        )
        .await??;
        let mut cdp = Cdp { ws, id: 0 };
        let target = cdp
            .call("Target.createTarget", json!({"url":"about:blank"}), None)
            .await?;
        let target_id = target["targetId"]
            .as_str()
            .context("missing browser target")?
            .to_owned();
        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId":target_id,"flatten":true}),
                None,
            )
            .await?;
        let session = attached["sessionId"]
            .as_str()
            .context("missing browser session")?
            .to_owned();
        let stored = cdp.call("Storage.getCookies", json!({}), None).await?;
        let cookies = stored["cookies"]
            .as_array()
            .context("missing browser cookies")?;
        let has_session = ["c_user", "xs"].iter().all(|name| {
            cookies.iter().any(|c| {
                c["name"].as_str() == Some(name) && c["domain"].as_str().is_some_and(meta_domain)
            })
        });
        if !has_session {
            let cookies: Vec<_> = jar.cookies().iter().filter(|c| meta_domain(&c.domain)).map(|c| json!({"name":c.name,"value":c.value,"domain":c.domain,"path":"/","secure":true})).collect();
            cdp.call("Storage.setCookies", json!({"cookies":cookies}), None)
                .await?;
        }
        let url = format!("https://www.messenger.com/e2ee/t/{chat_id}/");
        cdp.call("Page.navigate", json!({"url":url}), Some(&session))
            .await?;
        Ok(Self {
            _chrome: chrome,
            cdp,
            session,
            url,
        })
    }

    async fn reload(&mut self) -> Result<()> {
        let url = self.url.clone();
        self.cdp
            .call("Page.navigate", json!({"url":url}), Some(&self.session))
            .await?;
        Ok(())
    }

    async fn eval(&mut self, expression: &str) -> Result<Value> {
        let response = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true}),
                Some(&self.session),
            )
            .await?;
        Ok(response
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn state(&mut self) -> Result<PageState> {
        serde_json::from_value(self.eval(INSPECT).await?).context("invalid browser page state")
    }

    /// The message list renders in bursts, so wait for it to stop changing.
    async fn wait_ready(&mut self) -> Result<PageState> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last: Option<usize> = None;
        let mut stable = 0;
        loop {
            let state = self.state().await?;
            if matches!(state.state.as_str(), "login" | "recovery" | "checkpoint") {
                return Ok(state);
            }
            let settled = state.list && state.state != "loading";
            if settled {
                if last == Some(state.messages.len()) {
                    stable += 1;
                } else {
                    last = Some(state.messages.len());
                    stable = 0;
                }
                if stable >= 4 {
                    return Ok(state);
                }
            } else {
                last = None;
                stable = 0;
            }
            if Instant::now() >= deadline {
                bail!("conversation did not finish loading or its layout is unsupported");
            }
            sleep(Duration::from_millis(500)).await;
        }
    }
}

pub async fn read_messages(
    chat_id: &str,
    limit: usize,
    jar: &CookieJar,
) -> Result<Vec<BrowserMessage>> {
    let profile = login::profile_dir()?;
    let _lock = lock_profile(&profile)?;
    let mut page = Page::open(chat_id, jar).await?;
    let state = page.wait_ready().await?;
    finish(state, chat_id, limit)
}

const FOCUS: &str = r#"(() => {
 const boxes = [...document.querySelectorAll('div[contenteditable="true"], [contenteditable="true"][role="textbox"]')].filter(e => e.getClientRects().length);
 const composer = boxes.find(e => /message/i.test(e.getAttribute('aria-label') || '')) || boxes[boxes.length - 1];
 if (!composer) return {found:false};
 composer.focus();
 return {found:true, draft:(composer.innerText || '').trim().length};
})()"#;

/// A message carrying the sent text that was not already in the conversation.
fn confirmed<'a>(
    state: &'a PageState,
    text: &str,
    before: &HashSet<String>,
) -> Option<&'a BrowserMessage> {
    let needle = text.trim();
    if needle.is_empty() {
        return None;
    }
    state
        .messages
        .iter()
        .find(|m| !before.contains(&m.id) && m.text.contains(needle))
}

pub async fn send_message(chat_id: &str, text: &str, jar: &CookieJar) -> Result<BrowserMessage> {
    let text = text.trim();
    if text.is_empty() {
        bail!("message text is empty");
    }
    let profile = login::profile_dir()?;
    let _lock = lock_profile(&profile)?;
    let mut page = Page::open(chat_id, jar).await?;
    let state = page.wait_ready().await?;
    validate(&state, chat_id)?;
    let before: HashSet<String> = state.messages.iter().map(|m| m.id.clone()).collect();

    let focus = page.eval(FOCUS).await?;
    if focus["found"] != Value::Bool(true) {
        bail!("could not find the message composer");
    }
    if focus["draft"].as_u64().unwrap_or(0) > 0 {
        bail!("the message composer already holds a draft; clear it in Messenger first");
    }

    page.cdp
        .call(
            "Input.insertText",
            json!({"text": text}),
            Some(&page.session),
        )
        .await?;
    // The composer is a contenteditable; Enter is what actually sends.
    for event in [
        json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13,"text":"\r","unmodifiedText":"\r"}),
        json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"nativeVirtualKeyCode":13}),
    ] {
        page.cdp
            .call("Input.dispatchKeyEvent", event, Some(&page.session))
            .await?;
    }

    // The bubble renders before the encrypted payload uploads, and the list settles in bursts.
    sleep(Duration::from_secs(3)).await;
    page.reload().await?;
    let state = page.wait_ready().await?;
    validate(&state, chat_id)?;
    match confirmed(&state, text, &before) {
        Some(message) => Ok(BrowserMessage {
            id: message.id.clone(),
            text: message.text.clone(),
        }),
        None => bail!("the message did not appear in the conversation after sending"),
    }
}

fn meta_domain(domain: &str) -> bool {
    let d = domain.trim_start_matches('.');
    ["facebook.com", "messenger.com", "fb.com"]
        .iter()
        .any(|base| {
            d == *base
                || d.strip_suffix(base)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(id: &str) -> BrowserMessage {
        BrowserMessage {
            id: id.into(),
            text: "example".into(),
        }
    }
    #[test]
    fn rejects_other_thread_and_recovery() {
        assert!(
            finish(
                PageState {
                    path: "/e2ee/t/2".into(),
                    state: "ready".into(),
                    messages: vec![message("1")],
                    list: true
                },
                "1",
                20
            )
            .is_err()
        );
        assert!(
            finish(
                PageState {
                    path: "/e2ee/t/1".into(),
                    state: "recovery".into(),
                    messages: vec![],
                    list: true
                },
                "1",
                20
            )
            .is_err()
        );
    }
    #[test]
    fn reverses_deduplicates_and_limits() {
        let r = finish(
            PageState {
                path: "/e2ee/t/1".into(),
                state: "ready".into(),
                messages: vec![message("1"), message("2"), message("2"), message("3")],
                list: true,
            },
            "1",
            2,
        )
        .unwrap();
        assert_eq!(
            r.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["3", "2"]
        );
    }
    #[test]
    fn rejects_lookalike_domains() {
        assert!(meta_domain(".messenger.com"));
        assert!(!meta_domain("evilfacebook.com"));
        assert!(!meta_domain("facebook.com.example.org"));
    }
    #[test]
    fn rejects_unknown_layout() {
        assert!(
            finish(
                PageState {
                    path: "/e2ee/t/1".into(),
                    state: "unknown".into(),
                    messages: vec![],
                    list: true
                },
                "1",
                20
            )
            .is_err()
        );
    }
    #[test]
    fn confirms_the_sent_text() {
        let state = PageState {
            path: "/e2ee/t/1".into(),
            state: "ready".into(),
            list: true,
            messages: vec![BrowserMessage {
                id: "1".into(),
                text: "hello this is a test".into(),
            }],
        };
        let none = HashSet::new();
        assert_eq!(
            confirmed(&state, "  hello this is a test ", &none).map(|m| m.id.as_str()),
            Some("1")
        );
        assert!(confirmed(&state, "absent", &none).is_none());
        assert!(confirmed(&state, "   ", &none).is_none());
        let seen: HashSet<String> = ["1".to_string()].into_iter().collect();
        assert!(confirmed(&state, "hello this is a test", &seen).is_none());
    }

    #[test]
    fn lock_contention_is_reported() {
        let dir = std::env::temp_dir().join(format!("messenger-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = lock_profile(&dir).unwrap();
        assert!(lock_profile(&dir).is_err());
        drop(a);
        fs::remove_dir_all(dir).unwrap();
    }
}
