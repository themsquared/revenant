//! Client a revenant uses to muster at a Necropolis: register itself, discover
//! peers, publish signed artifacts, browse the catalog, pull + verify, and
//! attest a successful local re-verification.

use crate::artifact::Artifact;
use crate::ledger::Entry;
use anyhow::{bail, Context, Result};

/// A peer Necropolis's ledger head — its current length and chained hash.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LedgerHead {
    pub seq: i64,
    pub hash: String,
}

#[derive(Clone)]
pub struct NecropolisClient {
    base: String,
    http: reqwest::Client,
}

impl NecropolisClient {
    pub fn new(base: impl Into<String>) -> Self {
        NecropolisClient { base: base.into(), http: reqwest::Client::new() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    pub async fn register(&self, id: &str, endpoint: &str, capabilities: &[String]) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/register"))
            .json(&serde_json::json!({ "id": id, "endpoint": endpoint, "capabilities": capabilities }))
            .send()
            .await
            .context("registering with necropolis")?;
        if !resp.status().is_success() {
            bail!("register failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn peers(&self) -> Result<Vec<serde_json::Value>> {
        Ok(self.http.get(self.url("/peers")).send().await?.json().await?)
    }

    pub async fn publish(&self, artifact: &Artifact) -> Result<String> {
        let resp = self.http.post(self.url("/artifacts")).json(artifact).send().await?;
        if !resp.status().is_success() {
            bail!("publish failed: {}", resp.text().await.unwrap_or_default());
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    pub async fn list(&self, kind: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut url = self.url("/artifacts");
        if let Some(k) = kind {
            url.push_str(&format!("?kind={k}"));
        }
        Ok(self.http.get(url).send().await?.json().await?)
    }

    /// Fetch an artifact and verify its signature + content hash before
    /// returning it. A tampered or forged artifact is an error, not a value.
    pub async fn pull(&self, id: &str) -> Result<Artifact> {
        let resp = self.http.get(self.url(&format!("/artifacts/{id}"))).send().await?;
        if !resp.status().is_success() {
            bail!("pull failed: {}", resp.text().await.unwrap_or_default());
        }
        let artifact: Artifact = resp.json().await?;
        if !artifact.verify() {
            bail!("pulled artifact {id} FAILED verification — refusing to trust it");
        }
        Ok(artifact)
    }

    /// Report a successful local re-verification of an artifact's eval proof.
    pub async fn attest(&self, id: &str, verifier: &str, passed: bool) -> Result<()> {
        self.http
            .post(self.url(&format!("/artifacts/{id}/attest")))
            .json(&serde_json::json!({ "verifier": verifier, "passed": passed }))
            .send()
            .await
            .context("attesting")?;
        Ok(())
    }

    /// The peer's current ledger head — how far its history has advanced.
    pub async fn ledger_head(&self) -> Result<LedgerHead> {
        Ok(self.http.get(self.url("/ledger/head")).send().await?.json().await?)
    }

    /// Pull the peer's ledger entries with `seq > since`, in order. The bytes
    /// are transferred verbatim so the caller can recompute each hash and
    /// re-verify the chain locally before trusting a single entry.
    pub async fn ledger_since(&self, since: i64) -> Result<Vec<Entry>> {
        let resp = self.http.get(self.url(&format!("/ledger/since/{since}"))).send().await?;
        if !resp.status().is_success() {
            bail!("ledger pull failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp.json().await?)
    }
}
