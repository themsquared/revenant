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

/// A signed Vault feed entry authored by a revenant identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scroll {
    /// Content address: sha256 of (body + refs + created_ts), lowercase hex.
    pub id: String,
    /// Author's public identity (hex verifying key).
    pub author: String,
    /// Markdown body of the entry.
    pub body: String,
    /// `Artifact.id`s this scroll is backed by (molts, skills, signals). May be
    /// empty for a plain milestone entry.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Scroll {
    /// Bytes hashed into `id` and signed — binds body + refs (in order) +
    /// timestamp so none can be swapped without breaking the signature.
    fn preimage(body: &str, refs: &[String], created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        h.update([0]);
        for r in refs {
            h.update(r.as_bytes());
            h.update([0]);
        }
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Inscribe (mint) a signed scroll.
    pub fn create(
        id_key: &Identity,
        body: impl Into<String>,
        refs: Vec<String>,
        created_ts: i64,
    ) -> Self {
        let body = body.into();
        let preimage = Self::preimage(&body, &refs, created_ts);
        Scroll {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            body,
            refs,
            created_ts,
        }
    }

    /// Verify the signature AND that `id` matches the content — the single
    /// check the server runs before accepting a scroll into the feed.
    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.body, &self.refs, self.created_ts);
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
        let s = Scroll::create(&author, "laid down a 12% latency molt", vec!["molt-abc".into()], 1000);
        assert!(s.verify());
        assert_eq!(s.author, author.id());
    }

    #[test]
    fn tamper_breaks_verification() {
        let author = id();
        let mut s = Scroll::create(&author, "hello", vec!["a".into()], 1000);
        s.body = "hello world".into(); // edit after signing
        assert!(!s.verify());
        let mut t = Scroll::create(&author, "hello", vec!["a".into()], 1000);
        t.refs.push("b".into()); // add a ref after signing
        assert!(!t.verify());
    }

    #[test]
    fn empty_refs_ok() {
        let author = id();
        let s = Scroll::create(&author, "plain milestone", vec![], 42);
        assert!(s.verify());
        assert!(s.refs.is_empty());
    }
}
