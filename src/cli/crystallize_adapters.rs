//! Concrete HTTP and LLM adapters for the legacy H4 crystallization lane.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use super::crystallize::{ReviewClient, WorkflowSynthesizer};
use super::llm_extract::LlmExtractConfig;
use crate::domain::capability_capsule::{CapabilityCapsuleRecord, EditPendingRequest};

/// Per-call timeout. Crystallization reads several long capsules, so it gets
/// a longer budget than the O7(c) per-block extract.
const TIMEOUT_SECS: u64 = 180;
const MAX_GATEWAY_RESPONSE_BYTES: usize = 1024 * 1024;

const SYS_PROMPT: &str = "You distill a reusable procedure from several records of the SAME \
recurring task being executed multiple times (each record cites different commits/files — they are \
siblings, not duplicates). Output ONLY a JSON object: \
{\"title\": \"<one line naming the procedure>\", \"steps\": [\"<step>\", \"<step>\", ...]}. \
Each step must be one imperative line a future session can follow — concrete, ordered, and free of \
run-specific details (no commit shas, no one-off file names, no dates). Merge what the executions \
share; drop what was incidental to a single run. Aim for 3-10 steps. Write the steps in the same \
language the source records are written in. No prose, no markdown, no code fences — just the JSON \
object.";

pub struct HttpReviewClient {
    base_url: String,
    client: reqwest::Client,
    admin_token: Option<String>,
}

impl HttpReviewClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("crystallize HTTP client with no_proxy configuration"),
            admin_token: std::env::var("MEM_SKILL_REVIEWER_TOKEN")
                .or_else(|_| std::env::var("MEM_ADMIN_TOKEN"))
                .ok()
                .filter(|value| !value.is_empty()),
        }
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.admin_token.as_deref() {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

#[async_trait]
impl ReviewClient for HttpReviewClient {
    async fn list_pending(&self, tenant: &str) -> Result<Vec<CapabilityCapsuleRecord>> {
        let url = format!("{}/reviews/pending?tenant={}", self.base_url, tenant);
        self.authorize(self.client.get(&url))
            .send()
            .await
            .context("GET /reviews/pending")?
            .error_for_status()
            .context("GET /reviews/pending")?
            .json()
            .await
            .context("decode /reviews/pending")
    }

    async fn get_capsule(&self, tenant: &str, id: &str) -> Result<CapabilityCapsuleRecord> {
        let url = format!(
            "{}/capability_capsules/{}?tenant={}",
            self.base_url, id, tenant
        );
        let detail: crate::domain::capability_capsule::CapabilityCapsuleDetailResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("GET /capability_capsules/{id}")?
            .error_for_status()
            .context("GET /capability_capsules/{id}")?
            .json()
            .await
            .context("decode capsule detail")?;
        Ok(detail.capability_capsule)
    }

    async fn list_active_workflows(&self, tenant: &str) -> Result<Vec<CapabilityCapsuleRecord>> {
        #[derive(Deserialize)]
        struct ListResp {
            #[serde(default)]
            items: Vec<CapabilityCapsuleRecord>,
        }
        let url = format!("{}/capability_capsules/list", self.base_url);
        let resp: ListResp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "tenant": tenant,
                "capability_capsule_type": "workflow",
                "status": "active",
            }))
            .send()
            .await
            .context("POST /capability_capsules/list")?
            .error_for_status()
            .context("POST /capability_capsules/list")?
            .json()
            .await
            .context("decode capsule list")?;
        Ok(resp.items)
    }

    async fn edit_accept(&self, tenant: &str, req: EditPendingRequest) -> Result<String> {
        let url = format!("{}/reviews/pending/edit_accept", self.base_url);
        let body = serde_json::json!({
            "tenant": tenant,
            "capability_capsule_id": req.capability_capsule_id,
            "summary": req.summary,
            "content": req.content,
            "evidence": req.evidence,
            "code_refs": req.code_refs,
            "tags": req.tags,
        });
        let resp: crate::domain::capability_capsule::EditPendingResponse = self
            .authorize(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .context("POST /reviews/pending/edit_accept")?
            .error_for_status()
            .context("POST /reviews/pending/edit_accept")?
            .json()
            .await
            .context("decode edit_accept response")?;
        Ok(resp.capability_capsule.capability_capsule_id)
    }

    async fn link_supersedes(&self, _tenant: &str, new_id: &str, old_id: &str) -> Result<()> {
        let url = format!("{}/graph/edges", self.base_url);
        self.client
            .post(&url)
            .json(&serde_json::json!({
                "from_node_id": format!("capability_capsule:{new_id}"),
                "to_node_id": format!("capability_capsule:{old_id}"),
                "relation": "supersedes",
            }))
            .send()
            .await
            .context("POST /graph/edges")?
            .error_for_status()
            .context("POST /graph/edges")?;
        Ok(())
    }
}

/// The `llm_entry` gateway synthesizer (same wire shape as O7(c)).
pub struct GatewaySynthesizer {
    cfg: LlmExtractConfig,
}

impl GatewaySynthesizer {
    pub fn new(cfg: LlmExtractConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl WorkflowSynthesizer for GatewaySynthesizer {
    async fn synthesize(&self, prompt: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct ChatResp {
            choices: Vec<Choice>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: ChatMsg,
        }
        #[derive(Deserialize)]
        struct ChatMsg {
            content: Option<String>,
        }

        // `.no_proxy()` is load-bearing — an ambient HTTP(S)_PROXY would route
        // the internal-IP gateway call through the public proxy and 502.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build()
            .map_err(|e| e.to_string())?;

        let body = serde_json::json!({
            "model": self.cfg.model,
            "messages": [
                {"role": "system", "content": SYS_PROMPT},
                {"role": "user", "content": prompt},
            ],
            "temperature": 0.2,
            "caller": "mem-crystallize-h4",
        });
        let mut req = client
            .post(format!("{}/chat/completions", self.cfg.base))
            .header("Content-Type", "application/json")
            .json(&body);
        if !self.cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.cfg.api_key));
        }

        let mut resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("gateway status {status}"));
        }
        if resp
            .content_length()
            .is_some_and(|length| length > MAX_GATEWAY_RESPONSE_BYTES as u64)
        {
            return Err("gateway response exceeds byte limit".to_string());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|error| error.to_string())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_GATEWAY_RESPONSE_BYTES {
                return Err("gateway response exceeds byte limit".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        let parsed: ChatResp =
            serde_json::from_slice(&bytes).map_err(|e| format!("chat JSON: {e}"))?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }
}
