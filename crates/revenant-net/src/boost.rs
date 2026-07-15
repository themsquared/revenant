//! Boosts — spend credits to feature a quest or scroll higher on its board.
//!
//! A `Boost` is a signed statement that the booster is spending `amount` credits
//! to raise `target`'s ranking. The credits are *burned* (permanently removed
//! from the booster's balance, paid to no one) — the sink that gives credits a
//! use beyond staking bounties: pay for attention. The server debits the
//! booster's account, refuses a boost it can't afford, and orders the board by
//! total boost. Closed-loop and non-cashable, like every credit flow.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed credit spend that features a target (a Quest or Scroll id) higher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boost {
    /// The Quest id or Scroll id being boosted.
    pub target: String,
    /// Booster's public identity (hex verifying key).
    pub booster: String,
    /// Credits spent (burned) on this boost.
    pub amount: u64,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Boost {
    fn preimage(target: &str, amount: u64, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(target.as_bytes());
        h.update([0]);
        h.update(amount.to_le_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Mint a signed boost.
    pub fn create(id_key: &Identity, target: impl Into<String>, amount: u64, created_ts: i64) -> Self {
        let target = target.into();
        let preimage = Self::preimage(&target, amount, created_ts);
        Boost {
            sig: id_key.sign_hex(&preimage),
            booster: id_key.id(),
            target,
            amount,
            created_ts,
        }
    }

    /// Verify the signature is authentic for the stated booster + content.
    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.target, self.amount, self.created_ts);
        verify_hex(&self.booster, &preimage, &self.sig)
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
    fn boost_roundtrips_and_verifies() {
        let k = id();
        let b = Boost::create(&k, "quest-abc", 25, 1000);
        assert!(b.verify());
        assert_eq!(b.booster, k.id());
        assert_eq!(b.amount, 25);
    }

    #[test]
    fn tamper_breaks_verification() {
        let k = id();
        let mut b = Boost::create(&k, "quest-abc", 25, 1000);
        b.amount = 1_000_000; // inflate the boost after signing
        assert!(!b.verify());
        let mut c = Boost::create(&k, "quest-abc", 25, 1000);
        c.target = "other".into();
        assert!(!c.verify());
    }
}
