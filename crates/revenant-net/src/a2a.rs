//! Signed A2A envelopes — proving *which agent* sent a mesh message.
//!
//! A bearer token proves a caller knows a shared secret; it says nothing about
//! who they are, and it can't scale trust. The A2A envelope signs the exact
//! request body bytes plus a timestamp and nonce with the sender's Ed25519
//! identity — the same key that earns reputation on the Necropolis — so a
//! receiver can (1) authenticate the sender end-to-end, through any gateway or
//! proxy, (2) reject replays via the freshness window + nonce, and (3) scale
//! what the sender may trigger by that identity's standing on the network.
//!
//! Wire format: four HTTP headers alongside the JSON-RPC body —
//!   x-rev-agent: sender's hex verifying key
//!   x-rev-ts:    unix seconds the envelope was signed
//!   x-rev-nonce: random hex, single-use within the freshness window
//!   x-rev-sig:   Ed25519 signature (hex) over the preimage below
//!
//! Signing the raw body bytes (not a parsed/re-serialized form) keeps the
//! preimage byte-identical on both ends — no canonicalization to disagree on.

use crate::identity::{verify_hex, Identity};
use sha2::{Digest, Sha256};

pub const HDR_AGENT: &str = "x-rev-agent";
pub const HDR_TS: &str = "x-rev-ts";
pub const HDR_NONCE: &str = "x-rev-nonce";
pub const HDR_SIG: &str = "x-rev-sig";

/// How far an envelope timestamp may drift from the receiver's clock.
pub const A2A_FRESHNESS_SECS: i64 = 300;

fn preimage(body: &[u8], ts: i64, nonce: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(body);
    h.update([0]);
    h.update(ts.to_le_bytes());
    h.update([0]);
    h.update(nonce.as_bytes());
    h.finalize().to_vec()
}

/// Sign a request body for A2A. Returns the signature hex for `x-rev-sig`.
pub fn sign(id_key: &Identity, body: &[u8], ts: i64, nonce: &str) -> String {
    id_key.sign_hex(&preimage(body, ts, nonce))
}

/// Verify an envelope: is `sig` a valid signature by `agent` over exactly this
/// body + ts + nonce? Freshness and nonce reuse are the receiver's checks.
pub fn verify(agent: &str, body: &[u8], ts: i64, nonce: &str, sig: &str) -> bool {
    verify_hex(agent, &preimage(body, ts, nonce), sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn envelope_roundtrips_and_rejects_tampering() {
        let k = id();
        let body = br#"{"jsonrpc":"2.0","method":"message/send"}"#;
        let sig = sign(&k, body, 1000, "abc123");
        assert!(verify(&k.id(), body, 1000, "abc123", &sig));
        // Any mutation breaks it: body, ts, nonce, or claimed sender.
        assert!(!verify(&k.id(), b"{}", 1000, "abc123", &sig));
        assert!(!verify(&k.id(), body, 1001, "abc123", &sig));
        assert!(!verify(&k.id(), body, 1000, "abc124", &sig));
        let other = id();
        assert!(!verify(&other.id(), body, 1000, "abc123", &sig));
    }
}
