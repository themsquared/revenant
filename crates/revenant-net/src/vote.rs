//! Votes — a signed up/down signal on a Scroll or Reply.
//!
//! A bare vote is worthless against Sybils: one actor mints a thousand keys and
//! buries the signal. So a `Vote` is signed by an identity, and the *canonical*
//! tally the Necropolis serves collapses every voter key down to its verified
//! **account** (email-bound) before counting — one human, one vote per target.
//! The pure `tally()` here dedups per voter key (last-write-wins, so a voter can
//! flip or retract), and the server layer maps keys→accounts on top of it. That
//! keeps the byte-identical protocol piece testable in isolation while the
//! account collapse — the part that actually resists Sybils — lives where the
//! account bindings do.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed vote on a target (a `Scroll.id` or `Reply.id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    /// Content address: sha256 of (target + value + created_ts), lowercase hex.
    pub id: String,
    /// The `Scroll.id` or `Reply.id` being voted on.
    pub target: String,
    /// Voter's public identity (hex verifying key).
    pub voter: String,
    /// +1 up, -1 down, 0 retract (clamped on construction).
    pub value: i8,
    /// Unix seconds; stamped by the caller (deterministic, testable). Also the
    /// tie-breaker for last-write-wins when a voter changes their mind.
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl Vote {
    /// Bytes hashed into `id` and signed — binds the target, the value, and the
    /// timestamp so none can be swapped without breaking the signature.
    fn preimage(target: &str, value: i8, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(target.as_bytes());
        h.update([0]);
        h.update([value as u8]);
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Mint a signed vote. `value` is clamped to {-1, 0, +1}.
    pub fn create(
        id_key: &Identity,
        target: impl Into<String>,
        value: i8,
        created_ts: i64,
    ) -> Self {
        let target = target.into();
        let value = value.clamp(-1, 1);
        let preimage = Self::preimage(&target, value, created_ts);
        Vote {
            id: hex::encode(Sha256::digest(&preimage)),
            voter: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            target,
            value,
            created_ts,
        }
    }

    /// Verify the signature AND that `id` matches the content.
    pub fn verify(&self) -> bool {
        if !(-1..=1).contains(&self.value) {
            return false;
        }
        let preimage = Self::preimage(&self.target, self.value, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.voter, &preimage, &self.sig)
    }
}

/// The counted result for one target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub up: u32,
    pub down: u32,
    /// up − down.
    pub score: i64,
}

/// Tally the votes on `target`, deduped per voter (last-write-wins by
/// `created_ts`, then `id` for determinism). Only signatures that verify and
/// match this target count. A voter's latest `0` retracts their vote.
///
/// This dedups per *voter key*. The Necropolis wraps this with an account
/// collapse (map each key to its bound account, keep one vote per account)
/// before serving — that account layer is what makes the tally Sybil-resistant.
pub fn tally(votes: &[Vote], target: &str) -> Tally {
    use std::collections::HashMap;
    // voter -> (created_ts, id, value) of their latest valid vote on this target.
    let mut latest: HashMap<&str, (i64, &str, i8)> = HashMap::new();
    for v in votes {
        if v.target != target || !v.verify() {
            continue;
        }
        let cur = (v.created_ts, v.id.as_str(), v.value);
        match latest.get(v.voter.as_str()) {
            Some(&(ts, id, _)) if (ts, id) >= (cur.0, cur.1) => {}
            _ => {
                latest.insert(v.voter.as_str(), cur);
            }
        }
    }
    let mut t = Tally::default();
    for (_, _, value) in latest.values() {
        match value {
            1 => t.up += 1,
            -1 => t.down += 1,
            _ => {}
        }
    }
    t.score = t.up as i64 - t.down as i64;
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn vote_roundtrips_and_verifies() {
        let a = id();
        let v = Vote::create(&a, "scroll-abc", 1, 1000);
        assert!(v.verify());
        assert_eq!(v.voter, a.id());
        assert_eq!(v.value, 1);
    }

    #[test]
    fn value_is_clamped() {
        let a = id();
        assert_eq!(Vote::create(&a, "t", 5, 1).value, 1);
        assert_eq!(Vote::create(&a, "t", -9, 1).value, -1);
    }

    #[test]
    fn tamper_breaks_verification() {
        let a = id();
        let mut v = Vote::create(&a, "t", 1, 1000);
        v.value = -1; // flip after signing
        assert!(!v.verify());
        let mut w = Vote::create(&a, "t", 1, 1000);
        w.target = "other".into();
        assert!(!w.verify());
    }

    #[test]
    fn tally_dedups_per_voter_last_write_wins() {
        let a = id();
        let b = id();
        let votes = vec![
            Vote::create(&a, "t", 1, 1),  // a: up
            Vote::create(&a, "t", -1, 2), // a changes mind → down (this wins)
            Vote::create(&b, "t", 1, 1),  // b: up
        ];
        let t = tally(&votes, "t");
        assert_eq!(t, Tally { up: 1, down: 1, score: 0 });
    }

    #[test]
    fn zero_retracts() {
        let a = id();
        let votes = vec![Vote::create(&a, "t", 1, 1), Vote::create(&a, "t", 0, 2)];
        assert_eq!(tally(&votes, "t"), Tally { up: 0, down: 0, score: 0 });
    }

    #[test]
    fn other_targets_and_bad_sigs_excluded() {
        let a = id();
        let mut forged = Vote::create(&a, "t", 1, 1);
        forged.sig = "00".into();
        let votes = vec![forged, Vote::create(&a, "elsewhere", 1, 1)];
        assert_eq!(tally(&votes, "t"), Tally::default());
    }
}
