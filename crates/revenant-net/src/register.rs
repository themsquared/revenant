//! Signed presence registration — proving control of the keypair before the
//! directory will advertise an endpoint for it.
//!
//! Registration used to be unauthenticated: anyone could POST a pubkey → any
//! endpoint, so a bad actor could point another revenant's identity at a
//! machine it controls (discovery/endpoint spoofing). A `Registration` is
//! signed by the very key it advertises, so the directory stores presence only
//! for a caller that actually holds the private key. The timestamp lets the
//! server reject stale/replayed registrations.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed claim: "this key is reachable at `endpoint` with `capabilities`."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    /// The public identity being advertised (hex verifying key) — the signer.
    pub id: String,
    /// Where peers reach this agent (A2A endpoint).
    pub endpoint: String,
    pub capabilities: Vec<String>,
    /// Unix seconds; the server rejects registrations outside a freshness window.
    pub ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Registration {
    fn preimage(endpoint: &str, capabilities: &[String], ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(endpoint.as_bytes());
        h.update([0]);
        for c in capabilities {
            h.update(c.as_bytes());
            h.update([0]);
        }
        h.update([1]);
        h.update(ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(
        id_key: &Identity,
        endpoint: impl Into<String>,
        capabilities: Vec<String>,
        ts: i64,
    ) -> Self {
        let endpoint = endpoint.into();
        let preimage = Self::preimage(&endpoint, &capabilities, ts);
        Registration {
            id: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            endpoint,
            capabilities,
            ts,
        }
    }

    /// Authentic for the stated id + content. Callers separately enforce the
    /// timestamp freshness window.
    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.endpoint, &self.capabilities, self.ts);
        verify_hex(&self.id, &preimage, &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn registration_roundtrips_and_rejects_tampering() {
        let k = id();
        let r = Registration::create(&k, "https://node.example/a2a", vec!["chat".into()], 1000);
        assert!(r.verify());
        assert_eq!(r.id, k.id());
        // Repointing the endpoint under a different key must fail: the sig is
        // bound to the advertised identity.
        let mut spoof = r.clone();
        spoof.endpoint = "https://attacker.example/a2a".into();
        assert!(!spoof.verify());
        // Claiming someone else's id (without their key) fails.
        let other = id();
        let mut stolen = r.clone();
        stolen.id = other.id();
        assert!(!stolen.verify());
    }
}
