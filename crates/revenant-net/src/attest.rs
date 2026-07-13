//! Reproduction attestations — the network's proof-of-work vouch.
//!
//! An `Improvement` artifact carries its author's `eval_proof`. Trust in the
//! horde is earned by *re-verification*, not asserted: a peer pulls the
//! improvement, re-runs the eval suite on its OWN box, and — if it reproduces
//! the win — mints a signed `Attestation` bound to the artifact id. Because
//! reproduction costs real compute, a quorum of distinct attesters is
//! sybil-resistant in a way a bare vote never is (one actor can't cheaply
//! manufacture N independent reproductions).
//!
//! This module is the signed record + verification + quorum counting. The
//! actual eval re-run lives with the eval harness (it needs a running daemon);
//! this is the byte-identical protocol piece shared by every revenant and the
//! Necropolis server, so signatures verify the same on both sides.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A peer's signed statement that it independently re-ran an improvement's eval
/// proof and observed the claimed result (or failed to).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attestation {
    /// The `Artifact.id` (an Improvement) this attests to.
    pub artifact_id: String,
    /// The attesting peer's public identity (hex verifying key).
    pub attester: String,
    /// Did the peer reproduce the claimed win on its own box?
    pub reproduced: bool,
    /// Short human summary of what the peer saw, e.g. "12/12 pass, latency +11%".
    #[serde(default)]
    pub detail: String,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Attestation {
    /// Bytes hashed+signed — binds the artifact id, the verdict, the detail, and
    /// the timestamp so none can be swapped without breaking the signature. The
    /// attester is recovered from the key at verify time, so it isn't hashed in.
    fn preimage(artifact_id: &str, reproduced: bool, detail: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(artifact_id.as_bytes());
        h.update([0]);
        h.update([reproduced as u8]);
        h.update([0]);
        h.update(detail.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Mint a signed attestation for an artifact id.
    pub fn create(
        id_key: &Identity,
        artifact_id: impl Into<String>,
        reproduced: bool,
        detail: impl Into<String>,
        created_ts: i64,
    ) -> Self {
        let artifact_id = artifact_id.into();
        let detail = detail.into();
        let preimage = Self::preimage(&artifact_id, reproduced, &detail, created_ts);
        Attestation {
            sig: id_key.sign_hex(&preimage),
            attester: id_key.id(),
            artifact_id,
            reproduced,
            detail,
            created_ts,
        }
    }

    /// Verify the signature is authentic for the stated attester + content.
    pub fn verify(&self) -> bool {
        let preimage =
            Self::preimage(&self.artifact_id, self.reproduced, &self.detail, self.created_ts);
        verify_hex(&self.attester, &preimage, &self.sig)
    }
}

/// Count the DISTINCT peers who signed a valid, positive reproduction for
/// `artifact_id`. Only signatures that verify, target this artifact, report
/// `reproduced = true`, and (when `trusted` is non-empty) come from a trusted
/// attester are counted — each attester at most once. This is the raw quorum
/// input; reputation weighting is layered on top in a later phase.
pub fn distinct_reproductions(
    attestations: &[Attestation],
    artifact_id: &str,
    trusted: &[String],
) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for a in attestations {
        if a.artifact_id != artifact_id || !a.reproduced || !a.verify() {
            continue;
        }
        if !trusted.is_empty() && !trusted.contains(&a.attester) {
            continue;
        }
        seen.insert(a.attester.clone());
    }
    seen.len()
}

/// Is the reproduction quorum met — at least `required` distinct trusted peers?
pub fn quorum_met(
    attestations: &[Attestation],
    artifact_id: &str,
    trusted: &[String],
    required: usize,
) -> bool {
    distinct_reproductions(attestations, artifact_id, trusted) >= required
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn attestation_roundtrips_and_verifies() {
        let peer = id();
        let a = Attestation::create(&peer, "abc123", true, "12/12 pass, latency +11%", 1000);
        assert!(a.verify());
        assert_eq!(a.attester, peer.id());
    }

    #[test]
    fn tamper_breaks_verification() {
        let peer = id();
        let mut a = Attestation::create(&peer, "abc123", true, "ok", 1000);
        a.reproduced = false; // flip the verdict after signing
        assert!(!a.verify());
        let mut b = Attestation::create(&peer, "abc123", true, "ok", 1000);
        b.artifact_id = "different".into();
        assert!(!b.verify());
    }

    #[test]
    fn quorum_counts_distinct_trusted_positive_repros() {
        let p1 = id();
        let p2 = id();
        let p3 = id();
        let trusted = vec![p1.id(), p2.id(), p3.id()];
        let atts = vec![
            Attestation::create(&p1, "molt", true, "", 1),
            Attestation::create(&p1, "molt", true, "", 2), // same peer twice → counts once
            Attestation::create(&p2, "molt", true, "", 3),
            Attestation::create(&p3, "molt", false, "", 4), // failed repro → not counted
        ];
        assert_eq!(distinct_reproductions(&atts, "molt", &trusted), 2);
        assert!(quorum_met(&atts, "molt", &trusted, 2));
        assert!(!quorum_met(&atts, "molt", &trusted, 3));
    }

    #[test]
    fn untrusted_and_wrong_artifact_excluded() {
        let trusted_peer = id();
        let rando = id();
        let trusted = vec![trusted_peer.id()];
        let atts = vec![
            Attestation::create(&rando, "molt", true, "", 1), // untrusted
            Attestation::create(&trusted_peer, "other", true, "", 2), // wrong artifact
        ];
        assert_eq!(distinct_reproductions(&atts, "molt", &trusted), 0);
    }
}
