//! Typed client for the suzerain control plane REST API (web.rs).
//!
//! Thin wrapper over `suzerain_client::Client`'s HTTP transport
//! (docs/UNIFIED-AGENT-API-DESIGN.md §6 step 3) — this is what makes
//! suzerain-mcp a call-site adapter over the shared client instead of its
//! own independent reqwest wrapper. `server.rs`'s ~18 tool implementations
//! are untouched: they still call `get`/`get_query`/`post`/`delete` with
//! raw paths, exactly as before.

use anyhow::Result;
use serde_json::Value;

#[derive(Clone)]
pub struct ApiClient {
    client: suzerain_client::Client,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient").finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(base: String) -> Self {
        Self {
            client: suzerain_client::Client::http(&base),
        }
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.raw("GET", path, None).await
    }

    /// GET with query params (pairs with empty values are skipped).
    pub async fn get_query(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.raw("GET", &with_query(path, query), None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.raw("POST", path, Some(body)).await
    }

    pub async fn delete(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.raw("DELETE", &with_query(path, query), None).await
    }

    async fn raw(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        self.client.raw(method, path, body).await.map_err(|e| {
            // Preserve the API's {"error": "…"} message verbatim in the
            // anyhow chain — it carries operator-actionable guidance (e.g.
            // the secrets preflight's `suz secrets set …` remediation
            // commands) that server.rs's tool handlers surface to the LLM.
            match e {
                suzerain_client::Error::Http(_, msg) => anyhow::anyhow!("{msg}"),
                other => {
                    anyhow::anyhow!("reaching the control plane (is `suzerain run` up?): {other}")
                }
            }
        })
    }
}

fn with_query(path: &str, query: &[(&str, String)]) -> String {
    let pairs: Vec<String> = query
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", urlencoding(v)))
        .collect();
    if pairs.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", pairs.join("&"))
    }
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}
