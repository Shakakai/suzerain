//! Typed client for the suzerain control plane REST API (web.rs).

use anyhow::{bail, Context, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ApiClient {
    base: String,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(base: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client builds"),
        }
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    /// GET with query params (pairs with empty values are skipped).
    pub async fn get_query(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let query: Vec<(&str, &str)> = query
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        self.send(self.http.get(format!("{}{path}", self.base)).query(&query))
            .await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(&body))
            .await
    }

    pub async fn delete(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let query: Vec<(&str, &str)> = query
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        self.send(
            self.http
                .delete(format!("{}{path}", self.base))
                .query(&query),
        )
        .await
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req
            .send()
            .await
            .context("reaching the control plane (is `suzerain run` up?)")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The API's error shape is {"error": "…"}; surface it verbatim —
            // it carries operator-actionable guidance (e.g. the secrets
            // preflight's `suz secrets set …` remediation commands).
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{status}: {body}"));
            bail!("{msg}");
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }
}
