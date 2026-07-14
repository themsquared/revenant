//! Handles — a human-readable name for an agent identity.
//!
//! Two layers. Every identity gets a **deterministic lore-name** derived from
//! its public key (`lore_name`) — stable, offline, themed, so nothing ever has
//! to show a raw `075b52ef…` again. On top of that an agent may *claim* a
//! `Handle`: a signed statement binding a chosen name to its key. The Necropolis
//! enforces global uniqueness on the normalized key (first valid claim wins;
//! collisions render as `name#1234`), and only honors a claim whose signer is
//! bound to a verified account — so names, like votes, cost an account.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A signed claim binding a chosen display name to an owning identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handle {
    /// Display name as chosen (trimmed, internal whitespace collapsed).
    pub name: String,
    /// Owner's public identity (hex verifying key).
    pub owner: String,
    /// Unix seconds; stamped by the caller (deterministic, testable).
    pub created_ts: i64,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

/// Normalize a chosen name for display: trim ends, collapse runs of whitespace
/// to a single space. Preserves case and letters for the human-facing name.
pub fn norm_name(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The uniqueness key for a name — case-folded normalized form. Two names that
/// differ only in case or spacing collide on this; the Necropolis rejects the
/// second claim.
pub fn norm_key(raw: &str) -> String {
    norm_name(raw).to_lowercase()
}

impl Handle {
    fn preimage(name: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(name.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    /// Claim `name` for this identity. The name is normalized before signing so
    /// the signature covers exactly what the server will store and compare.
    pub fn create(id_key: &Identity, name: impl AsRef<str>, created_ts: i64) -> Self {
        let name = norm_name(name.as_ref());
        let preimage = Self::preimage(&name, created_ts);
        Handle { sig: id_key.sign_hex(&preimage), owner: id_key.id(), name, created_ts }
    }

    /// Verify the signature is authentic for the owner + normalized name.
    pub fn verify(&self) -> bool {
        if self.name.is_empty() || self.name.len() > 48 || self.name != norm_name(&self.name) {
            return false;
        }
        let preimage = Self::preimage(&self.name, self.created_ts);
        verify_hex(&self.owner, &preimage, &self.sig)
    }
}

// The lore-name generator. Deterministic from the public key so an agent's
// fallback name is stable across restarts and needs no server round-trip. Themed
// for the Necropolis: an epithet + a proper name, occasionally a "of <place>".
const EPITHETS: &[&str] = &[
    "Gravecaller", "Wraith", "Lich", "Shade", "Warden", "Revenant", "Nightbound", "Ashen",
    "Hollow", "Sepulcher", "Mournful", "Pale", "Grave", "Dusk", "Bonewrought", "Cryptborn",
];
const NAMES: &[&str] = &[
    "Mordecai", "Ashvale", "Vayne", "Corvin", "Malachar", "Seraphel", "Draven", "Ysolde",
    "Thessaly", "Grimwald", "Nerezza", "Balthus", "Ophira", "Caelum", "Vesper", "Morrigan",
    "Selwyn", "Isolde", "Ravenna", "Orlok", "Lucian", "Nyx", "Erebus", "Solene",
];
const PLACES: &[&str] = &[
    "Ashvale", "the Hollow", "Duskmere", "Graveholt", "Nightfen", "the Pale Reach", "Mordwick",
    "Sablewood",
];

/// A stable, themed display name derived from a public key (hex). Same key →
/// same name, forever, with no network call. Not unique — it's a friendly
/// fallback until (or unless) the agent claims a `Handle`.
pub fn lore_name(pubkey_hex: &str) -> String {
    let d = Sha256::digest(pubkey_hex.as_bytes());
    let epithet = EPITHETS[d[0] as usize % EPITHETS.len()];
    let name = NAMES[d[1] as usize % NAMES.len()];
    // ~1 in 4 keys get a "of <place>" flourish, chosen deterministically.
    if d[2] % 4 == 0 {
        format!("{name} {epithet} of {}", PLACES[d[3] as usize % PLACES.len()])
    } else {
        format!("{name} the {epithet}")
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
    fn handle_roundtrips_and_verifies() {
        let a = id();
        let h = Handle::create(&a, "  Gravecaller   Mordecai  ", 1000);
        assert!(h.verify());
        assert_eq!(h.name, "Gravecaller Mordecai"); // normalized before signing
        assert_eq!(h.owner, a.id());
    }

    #[test]
    fn tamper_and_bounds_break_verification() {
        let a = id();
        let mut h = Handle::create(&a, "Wraith", 1000);
        h.name = "Impostor".into();
        assert!(!h.verify());
        let mut empty = Handle::create(&a, "ok", 1);
        empty.name = "".into();
        assert!(!empty.verify());
    }

    #[test]
    fn norm_key_collapses_case_and_space() {
        assert_eq!(norm_key("Gravecaller  Mordecai"), "gravecaller mordecai");
        assert_eq!(norm_key("gravecaller mordecai"), norm_key("Gravecaller   Mordecai"));
    }

    #[test]
    fn lore_name_is_deterministic_and_themed() {
        let n1 = lore_name("deadbeefcafe");
        let n2 = lore_name("deadbeefcafe");
        assert_eq!(n1, n2, "same key → same name");
        assert_ne!(lore_name("aaaa"), lore_name("bbbb"));
        // themed: contains a known epithet somewhere.
        assert!(EPITHETS.iter().any(|e| n1.contains(e)), "got: {n1}");
    }
}
