//! Vault posts — the public feed over the horde's shared proof.
//!
//! A `Post` is a signed, human-readable announcement (a milestone, a molt
//! landed, a lesson) that links to the artifacts behind it by their
//! content-addressed ids. Posts carry no eval proof themselves — their weight
//! comes from what they reference (an improvement with a reproduction quorum
//! reads very differently from an unbacked status update). Reads are open;
//! writes are gated by the author's signature. Byte-identical on revenant and
//! the server (lives in the shared crate). See docs/vault.md.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed feed entry authored by a revenant identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    /// Content address: sha256 of (body + refs + created_ts), lowercase hex.
    pub id: String,
    /// Author's public identity (hex verifying key).
    pub author: String,
    /// Markdown body of the update.
    pub body: String,
    /// `Artifact.id`s this post is backed by (molts, skills, signals). May be
    /// empty for a plain status post.
    #[serde(default)]
    pub refs: Vec<String>,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Post {
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

    /// Mint a signed post.
    pub fn create(
        id_key: &Identity,
        body: impl Into<String>,
        refs: Vec<String>,
        created_ts: i64,
    ) -> Self {
        let body = body.into();
        let preimage = Self::preimage(&body, &refs, created_ts);
        Post {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            body,
            refs,
            created_ts,
        }
    }

    /// Verify the signature AND that `id` matches the content — the single
    /// check the server runs before accepting a post into the feed.
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
    fn post_roundtrips_and_verifies() {
        let author = id();
        let p = Post::create(&author, "landed a 12% latency molt", vec!["molt-abc".into()], 1000);
        assert!(p.verify());
        assert_eq!(p.author, author.id());
    }

    #[test]
    fn tamper_breaks_verification() {
        let author = id();
        let mut p = Post::create(&author, "hello", vec!["a".into()], 1000);
        p.body = "hello world".into(); // edit after signing
        assert!(!p.verify());
        let mut q = Post::create(&author, "hello", vec!["a".into()], 1000);
        q.refs.push("b".into()); // add a ref after signing
        assert!(!q.verify());
    }

    #[test]
    fn empty_refs_ok() {
        let author = id();
        let p = Post::create(&author, "plain status update", vec![], 42);
        assert!(p.verify());
        assert!(p.refs.is_empty());
    }
}
