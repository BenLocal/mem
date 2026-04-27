use anyhow::{anyhow, Result};
use reqwest::{Client, Method};
use serde::Serialize;
use serde_json::Value;

/// Percent-encode a single path segment (RFC 3986 unreserved set).
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[derive(Clone)]
pub struct MemHttpClient {
    base_url: String,
    http: Client,
}

impl MemHttpClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        let p = path.trim_start_matches('/');
        format!("{}/{}", self.base_url.trim_end_matches('/'), p)
    }

    pub async fn get_text(&self, path: &str) -> Result<String> {
        let res = self.http.get(self.url(path)).send().await?;
        let status = res.status();
        let text = res.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "mem HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 2000)
            ));
        }
        Ok(text)
    }

    pub async fn request_json<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Value> {
        self.request_json_with_query::<B>(method, path, body, &[]).await
    }

    pub async fn request_json_with_query<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        query: &[(&str, String)],
    ) -> Result<Value> {
        let mut req = self.http.request(method, self.url(path));
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req.send().await?;
        let status = res.status();
        let text = res.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "mem HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 2000)
            ));
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}
