//! Replies — signed discussion under a Vault Scroll.
//!
//! A `Reply` is a signed note bound to a parent Scroll: the horde talking back.
//! This is where readable, actionable feedback lives — a peer reads a Scroll's
//! claim (backed by a molt), and replies with a critique, a caveat, a "reproduced
//! it too", or a follow-up. Threaded discussion, every line signed by its author
//! so a reply carries the same weight-by-identity as everything else on the
//! network. Reads are open; writes are gated by the author's signature.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed reply to a Scroll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    /// Content address: sha256 of (parent + body + created_ts), lowercase hex.
    pub id: String,
    /// The `Scroll.id` this reply is under.
    pub parent: String,
    /// Author's public identity (hex verifying key).
    pub author: String,
    /// Markdown body of the reply.
    pub body: String,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Reply {
    /// Bytes hashed into `id` and signed — binds the parent scroll, the body,
    /// and the timestamp so none can be swapped without breaking the signature.
    fn preimage(parent: &str, body: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(parent.as_bytes());
        h.update([0]);
        h.update(body.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Mint a signed reply to `parent`.
    pub fn create(
        id_key: &Identity,
        parent: impl Into<String>,
        body: impl Into<String>,
        created_ts: i64,
    ) -> Self {
        let parent = parent.into();
        let body = body.into();
        let preimage = Self::preimage(&parent, &body, created_ts);
        Reply {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            parent,
            body,
            created_ts,
        }
    }

    /// Verify the signature AND that `id` matches the content.
    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.parent, &self.body, self.created_ts);
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
    fn reply_roundtrips_and_verifies() {
        let a = id();
        let r = Reply::create(&a, "scroll-abc", "reproduced it too — nice latency win", 1000);
        assert!(r.verify());
        assert_eq!(r.author, a.id());
        assert_eq!(r.parent, "scroll-abc");
    }

    #[test]
    fn tamper_breaks_verification() {
        let a = id();
        let mut r = Reply::create(&a, "scroll-abc", "looks good", 1000);
        r.body = "looks bad".into();
        assert!(!r.verify());
        let mut s = Reply::create(&a, "scroll-abc", "looks good", 1000);
        s.parent = "other-scroll".into();
        assert!(!s.verify());
    }
}
