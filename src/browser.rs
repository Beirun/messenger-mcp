use std::{
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
    let lock = OpenOptions::new().create(true).write(true).mode(0o600)
        .open(profile.join("messenger-mcp.lock"))?;
    lock.try_lock().context("browser profile is in use by messenger-mcp")?;
    if fs::symlink_metadata(profile.join("SingletonLock")).is_ok() {
        bail!("Chrome profile is in use or has a stale SingletonLock; close Chrome and check the profile");
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

struct Cdp { ws: Socket, id: u64 }
impl Cdp {
    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        let mut request = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session { request["sessionId"] = json!(session); }
        self.ws.send(Message::Text(request.to_string().into())).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let message = timeout(deadline.saturating_duration_since(Instant::now()), self.ws.next())
                .await.context("browser request timed out")?.context("browser connection closed")??;
            if let Message::Text(text) = message {
                let response: Value = serde_json::from_str(&text)?;
                if response.get("id").and_then(Value::as_u64) != Some(id) { continue; }
                if response.get("error").is_some() { bail!("browser rejected {method}"); }
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
struct PageState { path: String, state: String, messages: Vec<BrowserMessage> }

const INSPECT: &str = r#"(() => {
 const path = location.pathname.replace(/\/$/, '');
 const text = document.body?.innerText || '';
 const dialog = [...document.querySelectorAll('[role=dialog]')].filter(e => e.getClientRects().length).map(e => e.innerText).join(' ');
 if (location.pathname.includes('login') || document.querySelector('input[type=password]')) return {path,state:'login',messages:[]};
 if (/PIN to restore|one-time code|restore your chats/i.test(dialog)) return {path,state:'recovery',messages:[]};
 if (/checkpoint|confirm your identity/i.test(location.href + ' ' + dialog)) return {path,state:'checkpoint',messages:[]};
 const nodes = [...document.querySelectorAll('[data-scope="messages_table"][data-message-id]')].filter(e => e.getClientRects().length);
 const messages = nodes.map(e => ({id:e.getAttribute('data-message-id'),text:(e.innerText || e.getAttribute('aria-label') || '').slice(0,4000)})).filter(m => m.id && m.text);
 if (messages.length) return {path,state:'ready',messages};
 if (/loading messages/i.test(text)) return {path,state:'loading',messages:[]};
 return {path,state:'unknown',messages:[]};
})()"#;

fn finish(state: PageState, chat_id: &str, limit: usize) -> Result<Vec<BrowserMessage>> {
    if state.path != format!("/e2ee/t/{chat_id}") { bail!("browser opened a different conversation or redirected"); }
    match state.state.as_str() {
        "login" => bail!("Messenger requires sign-in; run messenger-mcp login"),
        "recovery" => bail!("Messenger requires encrypted history recovery in Chrome"),
        "checkpoint" => bail!("Messenger requires a security checkpoint in Chrome"),
        "loading" => bail!("timed out waiting for messages"),
        "ready" => {},
        _ => bail!("conversation layout was not recognized or messages did not load"),
    }
    let mut seen = std::collections::HashSet::new();
    let mut messages: Vec<_> = state.messages.into_iter().rev().filter(|m| seen.insert(m.id.clone())).take(limit).collect();
    messages.truncate(limit);
    Ok(messages)
}

pub async fn read_messages(chat_id: &str, limit: usize, jar: &CookieJar) -> Result<Vec<BrowserMessage>> {
    let profile = login::profile_dir()?;
    let _lock = lock_profile(&profile)?;
    let port_file = profile.join("DevToolsActivePort");
    if port_file.exists() { fs::remove_file(&port_file).context("removing stale DevTools port")?; }
    let child = Command::new(login::find_chrome()?)
        .arg("--headless=new").arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-debugging-port=0").arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run").arg("--no-default-browser-check").arg("about:blank")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().context("launching Chrome")?;
    let mut child = OwnedChrome {
        child,
        profile: profile.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let (port, path) = loop {
        if let Some(status) = child.child.try_wait()? { bail!("Chrome exited before opening DevTools ({status})"); }
        if let Ok(value) = fs::read_to_string(&port_file) {
            let mut lines = value.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.parse::<u16>() { break (port, path.to_owned()); }
            }
        }
        if Instant::now() >= deadline { bail!("Chrome did not open DevTools"); }
        sleep(Duration::from_millis(100)).await;
    };
    let (ws, _) = timeout(Duration::from_secs(10), connect_async(format!("ws://127.0.0.1:{port}{path}"))).await??;
    let mut cdp = Cdp { ws, id: 0 };
    let target = cdp.call("Target.createTarget", json!({"url":"about:blank"}), None).await?;
    let target_id = target["targetId"].as_str().context("missing browser target")?;
    let attached = cdp.call("Target.attachToTarget", json!({"targetId":target_id,"flatten":true}), None).await?;
    let session = attached["sessionId"].as_str().context("missing browser session")?.to_owned();
    let stored = cdp.call("Storage.getCookies", json!({}), None).await?;
    let cookies = stored["cookies"].as_array().context("missing browser cookies")?;
    let has_session = ["c_user", "xs"].iter().all(|name| cookies.iter().any(|c| c["name"].as_str() == Some(name) && c["domain"].as_str().is_some_and(meta_domain)));
    if !has_session {
        let cookies: Vec<_> = jar.cookies().iter().filter(|c| meta_domain(&c.domain)).map(|c| json!({"name":c.name,"value":c.value,"domain":c.domain,"path":"/","secure":true})).collect();
        cdp.call("Storage.setCookies", json!({"cookies":cookies}), None).await?;
    }
    cdp.call("Page.navigate", json!({"url":format!("https://www.messenger.com/e2ee/t/{chat_id}/")}), Some(&session)).await?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = cdp.call("Runtime.evaluate", json!({"expression":INSPECT,"returnByValue":true}), Some(&session)).await?;
        if let Some(value) = response.pointer("/result/value") {
            let state: PageState = serde_json::from_value(value.clone()).context("invalid browser page state")?;
            if state.state == "login" || state.state == "recovery" || state.state == "checkpoint" { return finish(state, chat_id, limit); }
            if state.state == "ready" { return finish(state, chat_id, limit); }
        }
        if Instant::now() >= deadline { bail!("conversation did not load or its layout is unsupported"); }
        sleep(Duration::from_millis(500)).await;
    }
}

fn meta_domain(domain: &str) -> bool {
    let d = domain.trim_start_matches('.');
    ["facebook.com", "messenger.com", "fb.com"].iter().any(|base| d == *base || d.strip_suffix(base).is_some_and(|prefix| prefix.ends_with('.')))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(id: &str) -> BrowserMessage { BrowserMessage { id: id.into(), text: "example".into() } }
    #[test] fn rejects_other_thread_and_recovery() {
        assert!(finish(PageState { path:"/e2ee/t/2".into(), state:"ready".into(), messages:vec![message("1")] }, "1", 20).is_err());
        assert!(finish(PageState { path:"/e2ee/t/1".into(), state:"recovery".into(), messages:vec![] }, "1", 20).is_err());
    }
    #[test] fn reverses_deduplicates_and_limits() {
        let r = finish(PageState { path:"/e2ee/t/1".into(), state:"ready".into(), messages:vec![message("1"),message("2"),message("2"),message("3")] }, "1", 2).unwrap();
        assert_eq!(r.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["3", "2"]);
    }
    #[test] fn rejects_lookalike_domains() {
        assert!(meta_domain(".messenger.com"));
        assert!(!meta_domain("evilfacebook.com"));
        assert!(!meta_domain("facebook.com.example.org"));
    }
    #[test] fn rejects_unknown_layout() {
        assert!(finish(PageState { path:"/e2ee/t/1".into(), state:"unknown".into(), messages:vec![] }, "1", 20).is_err());
    }
    #[test] fn lock_contention_is_reported() {
        let dir = std::env::temp_dir().join(format!("messenger-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = lock_profile(&dir).unwrap();
        assert!(lock_profile(&dir).is_err());
        drop(a);
        fs::remove_dir_all(dir).unwrap();
    }
}
