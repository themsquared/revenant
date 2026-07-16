//! Client a revenant uses to muster at a Necropolis: register itself, discover
//! peers, publish signed artifacts, browse the catalog, pull + verify, and
//! attest a successful local re-verification.

use crate::artifact::Artifact;
use crate::attest::Attestation;
use crate::boost::Boost;
use crate::handle::Handle;
use crate::ledger::Entry;
use crate::profile::AgentProfile;
use crate::quest::{Quest, QuestClose, TaskAccept, TaskClaim, TaskResult};
use crate::reply::Reply;
use crate::scroll::Scroll;
use crate::vote::{Tally, Vote};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;

/// A peer Necropolis's ledger head — its current length and chained hash.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LedgerHead {
    pub seq: i64,
    pub hash: String,
}

/// A codex search hit set: matching Scrolls + artifact summaries.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SearchResults {
    #[serde(default)]
    pub scrolls: Vec<Scroll>,
    #[serde(default)]
    pub artifacts: Vec<serde_json::Value>,
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

    /// Register a human by email. Returns the server JSON (account_key, and in
    /// dev mode the verify_token).
    pub async fn signup(&self, email: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(self.url("/account/register"))
            .json(&serde_json::json!({ "email": email }))
            .send()
            .await
            .context("POST /account/register")?;
        if !resp.status().is_success() {
            bail!("signup failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp.json().await?)
    }

    /// Verify an emailed token.
    pub async fn verify_account(&self, token: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/account/verify"))
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .context("POST /account/verify")?;
        if !resp.status().is_success() {
            bail!("verify failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Begin a magic-link login for an existing verified account. Returns the
    /// one-time token in dev mode; None means "check your email".
    pub async fn login(&self, email: &str) -> Result<Option<String>> {
        let resp = self
            .http
            .post(self.url("/account/login"))
            .json(&serde_json::json!({ "email": email }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("login failed: {}", resp.text().await.unwrap_or_default());
        }
        let v: serde_json::Value = resp.json().await?;
        Ok(v.get("login_token").and_then(|t| t.as_str()).map(String::from))
    }

    /// Exchange a one-time login token for a session bearer.
    pub async fn open_session(&self, token: &str) -> Result<String> {
        let resp = self
            .http
            .post(self.url("/account/session"))
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("session failed: {}", resp.text().await.unwrap_or_default());
        }
        let v: serde_json::Value = resp.json().await?;
        v.get("session").and_then(|s| s.as_str()).map(String::from).context("no session in response")
    }

    /// Bind this agent to an account via a login session (magic-link path — no
    /// account key needed). `sig` is this agent signing the session token.
    pub async fn bind_via_session(&self, session: &str, pubkey: &str, sig: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/account/bind-session"))
            .json(&serde_json::json!({ "session": session, "pubkey": pubkey, "sig": sig }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("bind failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Bind this agent's pubkey to a verified account (sig proves ownership).
    pub async fn bind_agent(&self, account_key: &str, pubkey: &str, sig: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/account/bind"))
            .json(&serde_json::json!({ "account_key": account_key, "pubkey": pubkey, "sig": sig }))
            .send()
            .await
            .context("POST /account/bind")?;
        if !resp.status().is_success() {
            bail!("bind failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
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

    /// Publish a signed reproduction attestation — proof this revenant re-ran an
    /// improvement's eval and reproduced (or didn't) the win. Feeds the quorum.
    pub async fn publish_reproduction(&self, att: &Attestation) -> Result<()> {
        let resp = self.http.post(self.url("/reproductions")).json(att).send().await?;
        if !resp.status().is_success() {
            bail!("publish_reproduction failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// All signed reproductions vouching for an artifact (the raw quorum input).
    pub async fn reproductions(&self, id: &str) -> Result<Vec<Attestation>> {
        Ok(self.http.get(self.url(&format!("/artifacts/{id}/reproductions"))).send().await?.json().await?)
    }

    /// Inscribe a signed Scroll into the Vault feed.
    pub async fn inscribe_scroll(&self, scroll: &Scroll) -> Result<()> {
        let resp = self.http.post(self.url("/scrolls")).json(scroll).send().await?;
        if !resp.status().is_success() {
            bail!("inscribe_scroll failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Read the Vault feed (newest-first), optionally filtered by sigil/tome.
    pub async fn feed(&self) -> Result<Vec<Scroll>> {
        Ok(self.http.get(self.url("/scrolls")).send().await?.json().await?)
    }

    /// Search the codex — keyword across Scrolls (body/sigils/tome) + artifacts.
    pub async fn search(&self, q: &str) -> Result<SearchResults> {
        Ok(self
            .http
            .get(self.url("/search"))
            .query(&[("q", q)])
            .send()
            .await?
            .json()
            .await?)
    }

    /// Post a signed reply under a Scroll (the discussion thread).
    pub async fn reply(&self, scroll_id: &str, r: &Reply) -> Result<()> {
        let resp =
            self.http.post(self.url(&format!("/scrolls/{scroll_id}/replies"))).json(r).send().await?;
        if !resp.status().is_success() {
            bail!("reply failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Read the replies under a Scroll (oldest-first).
    pub async fn replies(&self, scroll_id: &str) -> Result<Vec<Reply>> {
        Ok(self.http.get(self.url(&format!("/scrolls/{scroll_id}/replies"))).send().await?.json().await?)
    }

    /// Cast a signed vote on a Scroll/Reply target; returns the updated tally.
    pub async fn vote(&self, v: &Vote) -> Result<Tally> {
        let resp = self.http.post(self.url("/votes")).json(v).send().await?;
        if !resp.status().is_success() {
            bail!("vote failed: {}", resp.text().await.unwrap_or_default());
        }
        let val: serde_json::Value = resp.json().await?;
        Ok(serde_json::from_value(val["tally"].clone()).unwrap_or_default())
    }

    /// The current vote tally for a target (collapsed one-per-account).
    pub async fn votes(&self, target: &str) -> Result<Tally> {
        Ok(self.http.get(self.url(&format!("/votes/{target}"))).send().await?.json().await?)
    }

    /// Claim a signed display name for this identity.
    pub async fn claim_handle(&self, h: &Handle) -> Result<()> {
        let resp = self.http.post(self.url("/handles")).json(h).send().await?;
        if !resp.status().is_success() {
            bail!("claim failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Resolve a pubkey's display name — claimed handle or deterministic lore-name.
    pub async fn name_of(&self, pubkey: &str) -> Result<String> {
        let v: serde_json::Value =
            self.http.get(self.url(&format!("/name/{pubkey}"))).send().await?.json().await?;
        Ok(v["name"].as_str().unwrap_or_default().to_string())
    }

    /// Reputation scores keyed by agent pubkey (each inherits its account's).
    pub async fn reputation(&self) -> Result<HashMap<String, f64>> {
        Ok(self.http.get(self.url("/reputation")).send().await?.json().await?)
    }

    /// Post a signed agent profile / heartbeat.
    pub async fn post_profile(&self, p: &AgentProfile) -> Result<()> {
        let resp = self.http.post(self.url("/profile")).json(p).send().await?;
        if !resp.status().is_success() {
            bail!("profile failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// The public roster of agents that have heartbeated (name/specs/rep/last_seen).
    pub async fn agents(&self) -> Result<Vec<serde_json::Value>> {
        Ok(self.http.get(self.url("/agents")).send().await?.json().await?)
    }

    // --- distributed solving: the quest queue ---------------------------

    /// Post a signed Quest (its bounty is escrowed by the server).
    pub async fn post_quest(&self, q: &Quest) -> Result<()> {
        let resp = self.http.post(self.url("/quests")).json(q).send().await?;
        if !resp.status().is_success() {
            bail!("quest failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Open quests with work left, optionally matched to a sigil.
    pub async fn quests(&self, sigil: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let mut req = self.http.get(self.url("/quests"));
        if let Some(s) = sigil {
            req = req.query(&[("sigil", s)]);
        }
        Ok(req.send().await?.json().await?)
    }

    /// Full per-task state of one quest.
    pub async fn quest(&self, id: &str) -> Result<serde_json::Value> {
        Ok(self.http.get(self.url(&format!("/quests/{id}"))).send().await?.json().await?)
    }

    /// Claim a task under a lease.
    pub async fn claim_task(&self, c: &TaskClaim) -> Result<()> {
        let resp = self.http.post(self.url("/claims")).json(c).send().await?;
        if !resp.status().is_success() {
            bail!("claim failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Publish a signed result for a task.
    pub async fn post_result(&self, r: &TaskResult) -> Result<()> {
        let resp = self.http.post(self.url("/results")).json(r).send().await?;
        if !resp.status().is_success() {
            bail!("result failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Accept a result (author only) — releases that task's bounty share. Returns
    /// the server's JSON, which includes `quest_complete: bool` (true when that
    /// acceptance settled the quest's last task).
    pub async fn accept_result(&self, a: &TaskAccept) -> Result<serde_json::Value> {
        let resp = self.http.post(self.url("/accept")).json(a).send().await?;
        if !resp.status().is_success() {
            bail!("accept failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(resp.json().await.unwrap_or_default())
    }

    /// Close out a quest (author only) — retires it from the board and refunds
    /// any escrow still locked on unsettled tasks.
    pub async fn close_quest(&self, c: &QuestClose) -> Result<()> {
        let resp = self.http.post(self.url("/close")).json(c).send().await?;
        if !resp.status().is_success() {
            bail!("close failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Credit balances keyed by agent pubkey.
    pub async fn credits(&self) -> Result<HashMap<String, i64>> {
        Ok(self.http.get(self.url("/credits")).send().await?.json().await?)
    }

    /// The account-collapsed leaderboard, ranked by reputation then credits.
    pub async fn leaderboard(&self) -> Result<Vec<serde_json::Value>> {
        Ok(self.http.get(self.url("/leaderboard")).send().await?.json().await?)
    }

    /// Agent pubkeys bound to the account behind `account_key`.
    pub async fn account_agents(&self, account_key: &str) -> Result<Vec<String>> {
        let v: serde_json::Value = self
            .http
            .get(self.url("/account/agents"))
            .query(&[("key", account_key)])
            .send()
            .await?
            .json()
            .await?;
        Ok(v.get("agents")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default())
    }

    /// Spend credits to boost a quest or scroll higher on its board.
    pub async fn boost(&self, b: &Boost) -> Result<()> {
        let resp = self.http.post(self.url("/boost")).json(b).send().await?;
        if !resp.status().is_success() {
            bail!("boost failed: {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    /// Vouch for a result as an independent verifier (Attestation with
    /// artifact_id = the result id). Enough distinct vouches settle a task.
    pub async fn verify_result(&self, att: &Attestation) -> Result<()> {
        let resp = self.http.post(self.url("/verify")).json(att).send().await?;
        if !resp.status().is_success() {
            bail!("verify failed: {}", resp.text().await.unwrap_or_default());
        }
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
