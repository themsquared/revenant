//! A revenant's identity: an Ed25519 keypair generated once and kept on the
//! box. The public key IS the identity — a revenant signs everything it puts
//! on the network, so a peer can verify an artifact came from who it claims
//! and hasn't been tampered with, without trusting the directory. Self-
//! sovereign: no central authority mints identities.

use anyhow::{Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::path::Path;

pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Load the identity at `dir/ed25519.key`, generating (and persisting with
    /// 0600 perms) a fresh one on first run.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).context("creating identity dir")?;
        let path = dir.join("ed25519.key");
        if let Ok(hex_key) = std::fs::read_to_string(&path) {
            let bytes = hex::decode(hex_key.trim()).context("decoding identity key")?;
            let arr: [u8; 32] =
                bytes.as_slice().try_into().context("identity key must be 32 bytes")?;
            return Ok(Identity { signing: SigningKey::from_bytes(&arr) });
        }
        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        std::fs::write(&path, hex::encode(signing.to_bytes())).context("writing identity key")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Identity { signing })
    }

    /// The public identity — lowercase hex of the verifying key. This is the
    /// revenant's name on the network.
    pub fn id(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    /// A short, human-glanceable fingerprint (first 8 hex chars).
    pub fn fingerprint(&self) -> String {
        self.id()[..8].to_string()
    }

    /// Sign bytes; returns lowercase-hex signature.
    pub fn sign_hex(&self, bytes: &[u8]) -> String {
        hex::encode(self.signing.sign(bytes).to_bytes())
    }
}

/// Verify a hex signature against a hex public key. Any malformed input is a
/// verification failure, never a panic.
pub fn verify_hex(pubkey_hex: &str, bytes: &[u8], sig_hex: &str) -> bool {
    let Ok(pk) = hex::decode(pubkey_hex) else { return false };
    let Ok(pk_arr) = <[u8; 32]>::try_from(pk.as_slice()) else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk_arr) else { return false };
    let Ok(sig) = hex::decode(sig_hex) else { return false };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig.as_slice()) else { return false };
    vk.verify(bytes, &Signature::from_bytes(&sig_arr)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_reloads_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.id(), b.id(), "identity must be stable across loads");
        assert_eq!(a.id().len(), 64);
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        let sig = id.sign_hex(b"the horde rises");
        assert!(verify_hex(&id.id(), b"the horde rises", &sig));
        // Tampered payload → fails.
        assert!(!verify_hex(&id.id(), b"the horde falls", &sig));
        // Wrong key → fails.
        let other = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        assert!(!verify_hex(&other.id(), b"the horde rises", &sig));
        // Garbage → fails, no panic.
        assert!(!verify_hex("zz", b"x", "zz"));
    }
}
