use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn cookies_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("MESSENGER_COOKIES") {
        return Ok(path.into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("HOME is not set")?;
    Ok(base.join("messenger-mcp/cookies.json"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
}

#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if raw.trim_start().starts_with('[') {
            Self::parse_json(raw)
        } else {
            Self::parse_netscape(raw)
        }
    }

    fn parse_json(raw: &str) -> Result<Self> {
        let items: Vec<serde_json::Value> =
            serde_json::from_str(raw).context("parsing cookie JSON")?;
        let mut cookies = Vec::with_capacity(items.len());
        for item in items {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .context("cookie without a name")?;
            let value = item.get("value").and_then(|v| v.as_str()).unwrap_or("");
            let domain = item
                .get("domain")
                .and_then(|v| v.as_str())
                .unwrap_or(".facebook.com");
            cookies.push(Cookie {
                name: name.to_string(),
                value: value.to_string(),
                domain: domain.to_string(),
            });
        }
        Ok(Self { cookies })
    }

    fn parse_netscape(raw: &str) -> Result<Self> {
        let mut cookies = Vec::new();
        for line in raw.lines() {
            let line = line.trim_end_matches('\r');
            let line = if let Some(rest) = line.strip_prefix("#HttpOnly_") {
                rest
            } else if line.is_empty() || line.starts_with('#') {
                continue;
            } else {
                line
            };
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 7 {
                continue;
            }
            cookies.push(Cookie {
                domain: fields[0].to_string(),
                name: fields[5].to_string(),
                value: fields[6].to_string(),
            });
        }
        if cookies.is_empty() {
            bail!("no cookies found in file");
        }
        Ok(Self { cookies })
    }

    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
    }

    pub fn user_id(&self) -> Option<&str> {
        self.get("c_user")
    }

    pub fn missing_required(&self) -> Vec<&'static str> {
        ["c_user", "xs"]
            .into_iter()
            .filter(|name| self.get(name).is_none())
            .collect()
    }

    #[allow(dead_code)]
    pub fn header_for(&self, host: &str) -> String {
        self.cookies
            .iter()
            .filter(|c| domain_matches(&c.domain, host))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[allow(dead_code)]
fn domain_matches(cookie_domain: &str, host: &str) -> bool {
    let d = cookie_domain.trim_start_matches('.');
    host == d || host.ends_with(&format!(".{d}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"[
        {"domain": ".facebook.com", "name": "c_user", "value": "100012345"},
        {"domain": ".facebook.com", "name": "xs", "value": "abc123"},
        {"domain": ".messenger.com", "name": "locale", "value": "en_US"}
    ]"#;

    const NETSCAPE: &str = "# Netscape HTTP Cookie File\n\
        .facebook.com\tTRUE\t/\tTRUE\t0\tc_user\t100012345\n\
        .facebook.com\tTRUE\t/\tTRUE\t0\txs\tabc123\n\
        #HttpOnly_.facebook.com\tTRUE\t/\tTRUE\t0\tsb\tsbval\n";

    #[test]
    fn parses_json_export() {
        let jar = CookieJar::parse(JSON).unwrap();
        assert_eq!(jar.len(), 3);
        assert_eq!(jar.user_id(), Some("100012345"));
        assert!(jar.missing_required().is_empty());
    }

    #[test]
    fn parses_netscape_file() {
        let jar = CookieJar::parse(NETSCAPE).unwrap();
        assert_eq!(jar.len(), 3);
        assert_eq!(jar.user_id(), Some("100012345"));
        assert_eq!(jar.get("sb"), Some("sbval"));
    }

    #[test]
    fn reports_missing_required_cookies() {
        let jar = CookieJar::parse(r#"[{"name": "locale", "value": "en_US"}]"#).unwrap();
        assert_eq!(jar.missing_required(), vec!["c_user", "xs"]);
    }

    #[test]
    fn builds_cookie_header_per_host() {
        let jar = CookieJar::parse(JSON).unwrap();
        assert_eq!(
            jar.header_for("www.facebook.com"),
            "c_user=100012345; xs=abc123"
        );
        assert_eq!(jar.header_for("www.messenger.com"), "locale=en_US");
        assert_eq!(jar.header_for("example.com"), "");
    }
}
