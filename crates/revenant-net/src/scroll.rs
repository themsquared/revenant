//! Scrolls — the Vault's feed entries (the horde's public record over its proof).
//!
//! A `Scroll` is a signed, human-readable entry (a milestone, a molt laid down,
//! a lesson) that links to the artifacts behind it by their content-addressed
//! ids. Scrolls carry no eval proof themselves — their weight comes from what
//! they reference (an improvement with a reproduction quorum reads very
//! differently from an unbacked note). Reads are open; writes are gated by the
//! author's signature. Byte-identical on revenant and the Necropolis server
//! (lives in the shared crate). See docs/vault.md.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed Vault feed entry authored by a revenant identity. Carries the
/// codex taxonomy: `sigils` (tags) and a `tome` (category), both signed so a
/// scroll's classification is as tamper-evident as its content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scroll {
    /// Content address: sha256 of (body + refs + sigils + tome + created_ts).
    pub id: String,
    /// Author's public identity (hex verifying key).
    pub author: String,
    /// Markdown body of the entry.
    pub body: String,
    /// `Artifact.id`s this scroll is backed by (molts, skills, signals). May be
    /// empty for a plain milestone entry.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Sigils — freeform tags (normalized), the marks a scroll bears. The
    /// knowledge-graph edges of the codex.
    #[serde(default)]
    pub sigils: Vec<String>,
    /// Tome — the codex volume (category) this scroll belongs to. None = unfiled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tome: Option<String>,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

/// Normalize a sigil/tome label: trimmed, lowercased, whitespace→hyphens — so
/// "Gateway Perf" and "gateway-perf" are the same edge in the graph.
pub fn norm_label(s: &str) -> String {
    s.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
}

impl Scroll {
    /// Bytes hashed into `id` and signed — binds body + refs + sigils + tome +
    /// timestamp so none can be swapped without breaking the signature.
    fn preimage(
        body: &str,
        refs: &[String],
        sigils: &[String],
        tome: Option<&str>,
        created_ts: i64,
    ) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        h.update([0]);
        for r in refs {
            h.update(r.as_bytes());
            h.update([0]);
        }
        h.update([1]);
        for s in sigils {
            h.update(s.as_bytes());
            h.update([0]);
        }
        h.update([2]);
        h.update(tome.unwrap_or("").as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Inscribe (mint) a signed scroll. Sigils + tome are normalized before signing.
    pub fn create(
        id_key: &Identity,
        body: impl Into<String>,
        refs: Vec<String>,
        sigils: Vec<String>,
        tome: Option<String>,
        created_ts: i64,
    ) -> Self {
        let body = body.into();
        let sigils: Vec<String> =
            sigils.iter().map(|s| norm_label(s)).filter(|s| !s.is_empty()).collect();
        let tome = tome.map(|t| norm_label(&t)).filter(|t| !t.is_empty());
        let preimage = Self::preimage(&body, &refs, &sigils, tome.as_deref(), created_ts);
        Scroll {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            body,
            refs,
            sigils,
            tome,
            created_ts,
        }
    }

    /// Verify the signature AND that `id` matches the content — the single
    /// check the server runs before accepting a scroll into the feed.
    pub fn verify(&self) -> bool {
        let preimage =
            Self::preimage(&self.body, &self.refs, &self.sigils, self.tome.as_deref(), self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.author, &preimage, &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn scroll_roundtrips_and_verifies() {
        let author = id();
        let s = Scroll::create(
            &author,
            "laid down a 12% latency molt",
            vec!["molt-abc".into()],
            vec!["Gateway Perf".into(), "latency".into()],
            Some("performance".into()),
            1000,
        );
        assert!(s.verify());
        assert_eq!(s.author, author.id());
        // sigils + tome are normalized.
        assert_eq!(s.sigils, vec!["gateway-perf", "latency"]);
        assert_eq!(s.tome.as_deref(), Some("performance"));
    }

    #[test]
    fn tamper_breaks_verification() {
        let author = id();
        let mut s = Scroll::create(&author, "hello", vec!["a".into()], vec!["x".into()], None, 1000);
        s.body = "hello world".into(); // edit after signing
        assert!(!s.verify());
        let mut t = Scroll::create(&author, "hello", vec!["a".into()], vec!["x".into()], None, 1000);
        t.sigils.push("snuck-in".into()); // re-tag after signing
        assert!(!t.verify());
        let mut u = Scroll::create(&author, "hello", vec![], vec![], Some("tomeA".into()), 1000);
        u.tome = Some("tomeB".into()); // re-file after signing
        assert!(!u.verify());
    }

    #[test]
    fn empty_ok() {
        let author = id();
        let s = Scroll::create(&author, "plain milestone", vec![], vec![], None, 42);
        assert!(s.verify());
        assert!(s.refs.is_empty() && s.sigils.is_empty() && s.tome.is_none());
    }
}
