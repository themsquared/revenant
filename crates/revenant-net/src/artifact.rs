//! What flows across the network. Every artifact is signed by its author and
//! carries optional eval-proof, so a receiving revenant can (a) verify it is
//! authentic and untampered, and (b) re-run the proof locally before adopting
//! anything. Trust is earned by re-verification, not asserted by a feed.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// An Ascension molt: a code/config change that passed the eval bar.
    Improvement,
    /// A distilled skill (markdown) another revenant can drop in and use.
    Skill,
    /// A sandboxed WASM tool (bytes) — capability, verified before load.
    Plugin,
    /// Fast-moving operational intel (provider throttling, dead MCP, etc.).
    Signal,
}

/// A signed, self-describing unit of horde knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Content address: sha256 of (kind + title + payload), lowercase hex.
    pub id: String,
    pub kind: ArtifactKind,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Author's public identity (hex verifying key).
    pub author: String,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Base64 payload: skill markdown, wasm bytes, or a JSON signal blob.
    pub payload_b64: String,
    /// Optional eval proof (a revenant-evals JSON report) the receiver can
    /// re-run to earn trust. Signals may legitimately omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_proof: Option<serde_json::Value>,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

/// Pull `description:` out of a SKILL.md-style YAML frontmatter block so a
/// published skill carries a human description in the catalog (not the id/sig —
/// description is metadata, outside the signed preimage). None if absent.
pub fn frontmatter_description(text: &str) -> Option<String> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    for line in rest[..end].lines() {
        if let Some(v) = line.trim().strip_prefix("description:") {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

impl Artifact {
    /// The bytes that get hashed into `id` and signed — binds kind, title, and
    /// payload together so none can be swapped without breaking the signature.
    fn preimage(kind: ArtifactKind, title: &str, payload_b64: &str) -> Vec<u8> {
        let kind = serde_json::to_string(&kind).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(kind.as_bytes());
        h.update([0]);
        h.update(title.as_bytes());
        h.update([0]);
        h.update(payload_b64.as_bytes());
        h.finalize().to_vec()
    }

    /// Mint a signed artifact from raw payload bytes.
    pub fn create(
        id_key: &Identity,
        kind: ArtifactKind,
        title: impl Into<String>,
        description: impl Into<String>,
        payload: &[u8],
        eval_proof: Option<serde_json::Value>,
        created_ts: i64,
    ) -> Self {
        use base64::Engine;
        let title = title.into();
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        let preimage = Self::preimage(kind, &title, &payload_b64);
        Artifact {
            id: hex::encode(Sha256::digest(&preimage)),
            kind,
            title,
            description: description.into(),
            author: id_key.id(),
            created_ts,
            sig: id_key.sign_hex(&preimage),
            payload_b64,
            eval_proof,
        }
    }

    /// Decode the payload bytes.
    pub fn payload(&self) -> anyhow::Result<Vec<u8>> {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.decode(&self.payload_b64)?)
    }

    /// Verify the signature AND that `id` matches the content. Returns false
    /// on any mismatch — the single check a receiver runs before trusting
    /// metadata or re-running a proof.
    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(self.kind, &self.title, &self.payload_b64);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.author, &preimage, &self.sig)
    }

    /// Metadata-only view for catalog listings (no payload bytes).
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "kind": self.kind,
            "title": self.title,
            "description": self.description,
            "author": self.author,
            "created_ts": self.created_ts,
            "has_eval_proof": self.eval_proof.is_some(),
            "bytes": self.payload_b64.len(),
        })
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
    fn signed_artifact_verifies_and_roundtrips_payload() {
        let k = id();
        let a = Artifact::create(
            &k,
            ArtifactKind::Skill,
            "weather-arb",
            "a skill",
            b"# Weather Arb\nsteps...",
            Some(serde_json::json!({"passed": 6})),
            1_700_000_000,
        );
        assert!(a.verify());
        assert_eq!(a.payload().unwrap(), b"# Weather Arb\nsteps...");
        assert_eq!(a.author, k.id());
        assert!(a.summary()["has_eval_proof"].as_bool().unwrap());
    }

    #[test]
    fn tampering_breaks_verification() {
        let k = id();
        let mut a = Artifact::create(
            &k, ArtifactKind::Signal, "t", "d", b"x", None, 1,
        );
        // Swap the title without re-signing.
        a.title = "malicious".into();
        assert!(!a.verify());

        // Swap the payload.
        let mut b = Artifact::create(&k, ArtifactKind::Plugin, "t", "d", b"good", None, 1);
        use base64::Engine;
        b.payload_b64 = base64::engine::general_purpose::STANDARD.encode(b"evil");
        assert!(!b.verify());
    }

    #[test]
    fn forged_author_fails() {
        let k = id();
        let mut a = Artifact::create(&k, ArtifactKind::Improvement, "t", "d", b"x", None, 1);
        // Claim to be someone else while keeping the original signature.
        a.author = id().id();
        assert!(!a.verify());
    }
}
