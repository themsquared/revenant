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
use sha2::Digest;

pub const HDR_AGENT: &str = "x-rev-agent";
pub const HDR_TS: &str = "x-rev-ts";
pub const HDR_NONCE: &str = "x-rev-nonce";
pub const HDR_SIG: &str = "x-rev-sig";

/// How far an envelope timestamp may drift from the receiver's clock.
pub const A2A_FRESHNESS_SECS: i64 = 300;

/// Domain tag: binds a signature to the A2A envelope and no other record.
const DOMAIN_A2A: &[u8] = b"rev-a2a-envelope-v1";

fn preimage(domain: Option<&[u8]>, body: &[u8], ts: i64, nonce: &str) -> Vec<u8> {
    let mut h = crate::identity::preimage_hasher(domain);
    h.update(body);
    h.update([0]);
    h.update(ts.to_le_bytes());
    h.update([0]);
    h.update(nonce.as_bytes());
    h.finalize().to_vec()
}

/// Sign a request body for A2A. Returns the signature hex for `x-rev-sig`.
pub fn sign(id_key: &Identity, body: &[u8], ts: i64, nonce: &str) -> String {
    id_key.sign_hex(&preimage(Some(DOMAIN_A2A), body, ts, nonce))
}

/// Verify an envelope: is `sig` a valid signature by `agent` over exactly this
/// body + ts + nonce? Freshness and nonce reuse are the receiver's checks.
pub fn verify(agent: &str, body: &[u8], ts: i64, nonce: &str, sig: &str) -> bool {
    // PHASE 2 (SEC-5a): domain-tagged ONLY. The untagged fallback is gone.
    //
    // Safe to close here and nowhere else: an A2A envelope is live traffic. It is
    // never persisted and never replayed, so once every peer signs tagged
    // envelopes there is no historical signature that needs the old preimage.
    // Ledger-backed record types cannot do this — see horde.rs.
    verify_hex(agent, &preimage(Some(DOMAIN_A2A), body, ts, nonce), sig)
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

#[cfg(test)]
mod phase2_tests {
    use super::*;
    use crate::identity::Identity;

    /// Phase 2 closed the confusion window for A2A: an untagged signature — the
    /// shape a pre-SEC-5a peer produced — must now be REFUSED, not accepted.
    #[test]
    fn an_untagged_envelope_is_now_refused() {
        let key = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let (body, ts, nonce) = (b"{\"m\":\"hi\"}".as_slice(), 1_784_000_000i64, "abc123");

        // Legacy signer: no domain tag.
        let legacy_sig = key.sign_hex(&preimage(None, body, ts, nonce));
        assert!(
            !verify(&key.id(), body, ts, nonce, &legacy_sig),
            "an untagged envelope must no longer verify"
        );

        // The tagged form still round-trips.
        let good = sign(&key, body, ts, nonce);
        assert!(verify(&key.id(), body, ts, nonce, &good));

        // And tampering is still caught.
        assert!(!verify(&key.id(), b"{\"m\":\"bye\"}", ts, nonce, &good));
    }
}
