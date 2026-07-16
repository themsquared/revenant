//! Per-agent TLS material — the transport half of the identity.
//!
//! Each agent mints ONE persistent self-signed certificate. There is no CA:
//! the certificate is bound to the agent's Ed25519 identity by publishing its
//! SHA-256 fingerprint inside the identity-SIGNED `Registration` and
//! `AgentProfile` records (see docs/DESIGN-MTLS.md). A peer pins the presented
//! certificate against the latest signed claim — trust the key, not the
//! directory, extended down to the wire.
//!
//! ECDSA P-256, not Ed25519, for the certificate keypair: every TLS stack
//! verifies P-256 certs, while Ed25519 certificate support is still spotty.
//! The *identity* stays Ed25519; the cert key is disposable transport material
//! (rotation = mint a new one + heartbeat the new fingerprint).

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// On-disk names inside the identity dir, alongside the Ed25519 key.
const CERT_FILE: &str = "tls.crt"; // PEM certificate
const KEY_FILE: &str = "tls.key"; // PEM PKCS#8 private key

/// An agent's TLS material: PEM cert + key, and the DER fingerprint peers pin.
pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    /// SHA-256 of the certificate DER, lowercase hex — the pinned value.
    pub fingerprint: String,
}

/// Fingerprint a certificate: SHA-256 over the DER bytes, lowercase hex.
pub fn fingerprint_der(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Fingerprint a PEM certificate (first CERTIFICATE block).
pub fn fingerprint_pem(pem: &str) -> Result<String> {
    let der = pem_to_der(pem).context("no CERTIFICATE block in PEM")?;
    Ok(fingerprint_der(&der))
}

/// Extract the first CERTIFICATE block's DER from a PEM string.
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let start = pem.find("-----BEGIN CERTIFICATE-----")?;
    let end = pem.find("-----END CERTIFICATE-----")?;
    let b64: String = pem[start + 27..end].chars().filter(|c| !c.is_whitespace()).collect();
    base64_decode(&b64)
}

/// Minimal base64 decode (standard alphabet, padded) — avoids a dependency.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = ALPHA.iter().position(|&a| a == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Load the agent's TLS material from `dir`, minting it on first use. The
/// same material persists across restarts so the published fingerprint stays
/// stable; rotation is deliberate (delete/replace + re-publish), never silent.
pub fn load_or_create(dir: &Path) -> Result<TlsMaterial> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        let fingerprint = fingerprint_pem(&cert_pem)?;
        return Ok(TlsMaterial { cert_pem, key_pem, fingerprint });
    }

    // Mint: self-signed, long-lived (rotation is by supersession of the signed
    // fingerprint, not by expiry pressure), generic SANs (peers pin the
    // fingerprint, not the name).
    let mut params = rcgen::CertificateParams::new(vec!["revenant".to_string()])
        .context("building certificate params")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "revenant-agent");
    let key = rcgen::KeyPair::generate().context("generating P-256 keypair")?;
    let cert = params.self_signed(&key).context("self-signing certificate")?;

    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();
    std::fs::write(&cert_path, &cert_pem)?;
    std::fs::write(&key_path, &key_pem)?;
    // Key is secret material: owner-only, like the Ed25519 key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }
    let fingerprint = fingerprint_der(cert.der());
    Ok(TlsMaterial { cert_pem, key_pem, fingerprint })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mints_once_and_fingerprint_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create(dir.path()).unwrap();
        assert_eq!(a.fingerprint.len(), 64);
        assert!(a.cert_pem.contains("BEGIN CERTIFICATE"));
        // Reload returns the SAME material — the published pin must not drift.
        let b = load_or_create(dir.path()).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.cert_pem, b.cert_pem);
        // PEM and DER fingerprints agree.
        assert_eq!(fingerprint_pem(&a.cert_pem).unwrap(), a.fingerprint);
    }

    #[test]
    fn distinct_agents_get_distinct_certs() {
        let a = load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let b = load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }
}
