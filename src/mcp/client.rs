use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::{Client, Method};
use serde::Serialize;
use serde_json::Value;

pub const MCP_HTTP_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerRequestError {
    Retryable,
    Correctable,
    Terminal,
}

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
    admin_token: Option<String>,
}

impl MemHttpClient {
    pub fn new(base_url: String, admin_token: Option<String>) -> Self {
        Self {
            base_url,
            // Admin bearer credentials must never ride ambient public proxy
            // settings when the configured mem service is local/private.
            http: Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(MCP_HTTP_TIMEOUT_MS))
                .build()
                .expect("reqwest client with no_proxy configuration"),
            admin_token,
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
        self.request_json_with_query::<B>(method, path, body, &[])
            .await
    }

    pub async fn request_admin_json<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Value> {
        self.request_admin_json_with_query(method, path, body, &[])
            .await
    }

    pub async fn request_admin_json_with_query<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        query: &[(&str, String)],
    ) -> Result<Value> {
        let token = self.admin_token.as_deref().ok_or_else(|| {
            anyhow!("MEM_SKILL_REVIEWER_TOKEN or MEM_ADMIN_TOKEN is required for review tools")
        })?;
        let mut req = self.http.request(method, self.url(path)).bearer_auth(token);
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        decode_json_response(req.send().await?).await
    }

    pub async fn request_compiler_json<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Value, CompilerRequestError> {
        let token = self
            .admin_token
            .as_deref()
            .ok_or(CompilerRequestError::Terminal)?;
        let mut request = self.http.request(method, self.url(path)).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| CompilerRequestError::Retryable)?;
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 429 || status.is_server_error() {
                return Err(CompilerRequestError::Retryable);
            }
            if matches!(status.as_u16(), 400 | 422) {
                return Err(CompilerRequestError::Correctable);
            }
            if status.as_u16() == 409
                && response
                    .content_length()
                    .is_none_or(|length| length <= 4_096)
            {
                let body = response.json::<Value>().await.unwrap_or_default();
                let message = body
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if message.contains("already in progress") || message.contains("is busy") {
                    return Err(CompilerRequestError::Retryable);
                }
            }
            return Err(CompilerRequestError::Terminal);
        }
        response
            .json()
            .await
            .map_err(|_| CompilerRequestError::Retryable)
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
        decode_json_response(req.send().await?).await
    }
}

async fn decode_json_response(res: reqwest::Response) -> Result<Value> {
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

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}
