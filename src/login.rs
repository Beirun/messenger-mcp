use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const LOGIN_URL: &str = "https://www.messenger.com/";

pub async fn run() -> Result<()> {
    let chrome = find_chrome()?;
    let profile = profile_dir()?;
    fs::create_dir_all(&profile)?;
    let _lock = crate::browser::lock_profile(&profile)?;

    let mut child = launch_chrome(&chrome, &profile)?;
    eprintln!(
        "waiting for sign-in in the Chrome window (up to {}s)",
        timeout_secs()
    );

    let outcome = capture(&profile).await;
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

async fn capture(profile: &Path) -> Result<()> {
    let port_file = profile.join("DevToolsActivePort");
    let (port, path) = wait_for_devtools(&port_file)?;
    let mut ws = connect_ws(port, &path).await?;

    let outcome = poll_and_save(&mut ws)
        .await
        .context("browser closed before sign-in completed");
    let _ = ws
        .send(Message::Text(
            json!({"id": 0, "method": "Browser.close"})
                .to_string()
                .into(),
        ))
        .await;
    outcome
}

async fn poll_and_save(ws: &mut Ws) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs());
    let mut id = 0u64;
    let cookies = loop {
        let cookies = cdp_cookies(ws, &mut id).await?;
        if is_logged_in(&cookies) {
            break cookies;
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for sign-in");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };

    let (saved, count) = save_cookies(&cookies)?;
    let user_id = cookies
        .iter()
        .find(|c| c.get("name").and_then(Value::as_str) == Some("c_user"))
        .and_then(|c| c.get("value").and_then(Value::as_str))
        .unwrap_or("unknown");
    println!(
        "saved {count} cookies for user {user_id} to {}",
        saved.display()
    );
    Ok(())
}

fn timeout_secs() -> u64 {
    std::env::var("MESSENGER_LOGIN_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

fn launch_chrome(chrome: &Path, profile: &Path) -> Result<Child> {
    let _ = fs::remove_file(profile.join("DevToolsActivePort"));
    Command::new(chrome)
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-blink-features=AutomationControlled")
        .arg(LOGIN_URL)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launching {}", chrome.display()))
}

fn wait_for_devtools(port_file: &Path) -> Result<(u16, String)> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(contents) = fs::read_to_string(port_file) {
            let mut lines = contents.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if let Ok(port) = port.trim().parse::<u16>() {
                    return Ok((port, path.trim().to_string()));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("Chrome did not open a DevTools port");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

async fn connect_ws(port: u16, path: &str) -> Result<Ws> {
    let url = format!("ws://127.0.0.1:{port}{path}");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match connect_async(&url).await {
            Ok((ws, _)) => return Ok(ws),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(err).context("connecting to DevTools");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn cdp_cookies(ws: &mut Ws, id: &mut u64) -> Result<Vec<Value>> {
    *id += 1;
    let request = json!({"id": id, "method": "Storage.getCookies"});
    ws.send(Message::Text(request.to_string().into()))
        .await
        .context("sending CDP request")?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let message = tokio::time::timeout_at(deadline, ws.next())
            .await
            .context("CDP response timed out")?
            .context("DevTools socket closed")?
            .context("DevTools socket error")?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(text.as_str()).context("parsing CDP response")?;
        if value.get("id").and_then(Value::as_u64) != Some(*id) {
            continue;
        }
        if let Some(err) = value.get("error") {
            bail!("CDP error: {err}");
        }
        let cookies = value
            .pointer("/result/cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        return Ok(cookies);
    }
}

fn is_logged_in(cookies: &[Value]) -> bool {
    let has = |name: &str| {
        cookies
            .iter()
            .any(|c| c.get("name").and_then(Value::as_str) == Some(name))
    };
    has("c_user") && has("xs")
}

fn save_cookies(cookies: &[Value]) -> Result<(PathBuf, usize)> {
    let path = crate::auth::cookies_path()?;
    let count = write_cookies(cookies, &path)?;
    Ok((path, count))
}

fn write_cookies(cookies: &[Value], path: &Path) -> Result<usize> {
    let relevant: Vec<Value> = cookies
        .iter()
        .filter(|c| {
            c.get("domain").and_then(Value::as_str).is_some_and(|d| {
                d.contains("facebook.com") || d.contains("messenger.com") || d.contains("fb.com")
            })
        })
        .map(|c| {
            json!({
                "domain": c.get("domain").and_then(Value::as_str).unwrap_or(""),
                "name": c.get("name").and_then(Value::as_str).unwrap_or(""),
                "value": c.get("value").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();
    if relevant.is_empty() {
        bail!("no facebook or messenger cookies found");
    }

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_vec_pretty(&relevant)?)
        .with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(relevant.len())
}

pub(crate) fn profile_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("HOME is not set")?;
    Ok(base.join("messenger-mcp/chrome-profile"))
}

pub(crate) fn find_chrome() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MESSENGER_CHROME") {
        return Ok(path.into());
    }
    if let Some(path) = agent_browser_chrome() {
        return Ok(path);
    }
    for name in ["google-chrome", "chromium", "chromium-browser", "chrome"] {
        if let Some(path) = find_in_path(name) {
            return Ok(path);
        }
    }
    bail!("no Chrome binary found; set MESSENGER_CHROME to one")
}

fn agent_browser_chrome() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".agent-browser/browsers");
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        let Some(version) = name.strip_prefix("chrome-") else {
            continue;
        };
        let binary = entry.path().join("chrome");
        if !binary.is_file() {
            continue;
        }
        let parts: Vec<u64> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        if best.as_ref().is_none_or(|(current, _)| parts > *current) {
            best = Some((parts, binary));
        }
    }
    best.map(|(_, path)| path)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cookie(name: &str, value: &str, domain: &str) -> Value {
        json!({"name": name, "value": value, "domain": domain})
    }

    #[test]
    fn writes_only_meta_domains_with_private_permissions() {
        let dir = std::env::temp_dir().join(format!("messenger-mcp-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cookies.json");

        let cookies = vec![
            cookie("c_user", "1000", ".facebook.com"),
            cookie("xs", "secret", ".messenger.com"),
            cookie("session", "other", "example.com"),
        ];
        write_cookies(&cookies, &path).unwrap();

        let saved: Vec<Value> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.len(), 2);
        assert!(saved.iter().all(|c| {
            let d = c["domain"].as_str().unwrap();
            d.contains("facebook.com") || d.contains("messenger.com")
        }));

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_login_from_cookies() {
        let cookies = vec![
            cookie("c_user", "1000", ".facebook.com"),
            cookie("xs", "secret", ".facebook.com"),
        ];
        assert!(is_logged_in(&cookies));
        assert!(!is_logged_in(&cookies[..1]));
    }
}
