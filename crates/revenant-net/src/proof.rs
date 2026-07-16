//! Signed read proofs — authenticating WHO is reading, not just who writes.
//!
//! Every write on the network is a signed record, but reads of the *private*
//! horde board used to be scoped by a bare pubkey in the querystring — knowing
//! a pubkey was enough to list that account's board. A read proof closes that:
//! the reader signs (resource path, timestamp, nonce) with its agent key, and
//! the server scopes the read to the account of the key that actually signed.
//! A stranger can then only ever read their own (empty) board — privacy by
//! construction, not by obscurity of a pubkey.
//!
//! Same wire shape as A2A envelopes (x-rev-agent/-ts/-nonce/-sig headers), but
//! the preimage is domain-separated so an A2A body signature can never be
//! replayed as a read proof or vice versa.

use crate::identity::{verify_hex, Identity};
use sha2::{Digest, Sha256};

/// Domain tag: keeps read-proof signatures disjoint from every other preimage.
const DOMAIN: &[u8] = b"rev-read-proof-v1";

/// How far a proof's timestamp may drift from the verifier's clock.
pub const PROOF_FRESHNESS_SECS: i64 = 300;

fn preimage(resource: &str, ts: i64, nonce: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update([0]);
    h.update(resource.as_bytes());
    h.update([0]);
    h.update(ts.to_le_bytes());
    h.update([0]);
    h.update(nonce.as_bytes());
    h.finalize().to_vec()
}

/// Sign a read of `resource` (the exact request path, e.g. "/horde/tasks").
pub fn sign(id_key: &Identity, resource: &str, ts: i64, nonce: &str) -> String {
    id_key.sign_hex(&preimage(resource, ts, nonce))
}

/// Verify a read proof. Freshness and nonce reuse are the verifier's checks.
pub fn verify(agent: &str, resource: &str, ts: i64, nonce: &str, sig: &str) -> bool {
    verify_hex(agent, &preimage(resource, ts, nonce), sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn proof_roundtrips_and_rejects_tampering() {
        let k = id();
        let sig = sign(&k, "/horde/tasks", 1000, "n1");
        assert!(verify(&k.id(), "/horde/tasks", 1000, "n1", &sig));
        // Resource, ts, nonce, or claimed signer changes all invalidate it.
        assert!(!verify(&k.id(), "/horde/runs/x", 1000, "n1", &sig));
        assert!(!verify(&k.id(), "/horde/tasks", 1001, "n1", &sig));
        assert!(!verify(&k.id(), "/horde/tasks", 1000, "n2", &sig));
        let other = id();
        assert!(!verify(&other.id(), "/horde/tasks", 1000, "n1", &sig));
    }

    #[test]
    fn a2a_signature_is_not_a_valid_read_proof() {
        // Domain separation: signing the same string via the A2A envelope must
        // not verify as a read proof.
        let k = id();
        let a2a_sig = crate::a2a::sign(&k, b"/horde/tasks", 1000, "n1");
        assert!(!verify(&k.id(), "/horde/tasks", 1000, "n1", &a2a_sig));
    }
}
