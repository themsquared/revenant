//! Agent profiles — a signed heartbeat that says "this agent is alive, and here
//! is what it is." The data behind the "My Horde" dashboard: an owner logs in
//! and sees their agents — live or stale, their machine specs, their
//! capabilities, their reputation. Like everything else on the network it is
//! signed by the agent itself, so the directory vouches for nothing: the specs
//! an agent advertises are its own signed claim.
//!
//! Reporting is opt-in (the daemon only heartbeats when the owner enables it),
//! and specs are deliberately coarse — os/arch/core-count/RAM/GPU — never
//! anything that identifies the machine on a network.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Coarse machine specs an agent advertises. Optional-ish: unknown fields are
/// empty/zero rather than absent, so the signed preimage is stable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineSpecs {
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub ram_mb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<String>,
}

/// A signed agent profile / heartbeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    /// The agent's public identity (hex verifying key) — recovered from the sig.
    pub agent: String,
    /// Display name the agent chose (mirrors its Handle claim, if any).
    #[serde(default)]
    pub name: String,
    pub specs: MachineSpecs,
    /// What the agent can do (tool/skill capabilities it advertises).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Unix seconds this heartbeat was stamped — also the liveness clock.
    pub created_ts: i64,
    /// SHA-256 fingerprint of this agent's TLS certificate (lowercase hex) —
    /// the identity-signed pin peers verify a TLS session against (SEC-4).
    /// Optional so pre-mTLS records and clients keep verifying unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_fp: Option<String>,
    /// Ed25519 signature (hex) over the signing preimage.
    pub sig: String,
}

impl AgentProfile {
    fn preimage(
        name: &str,
        specs: &MachineSpecs,
        capabilities: &[String],
        created_ts: i64,
        tls_fp: Option<&str>,
    ) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(name.as_bytes());
        h.update([0]);
        h.update(specs.os.as_bytes());
        h.update([0]);
        h.update(specs.arch.as_bytes());
        h.update([0]);
        h.update(specs.cpus.to_le_bytes());
        h.update(specs.ram_mb.to_le_bytes());
        h.update([0]);
        h.update(specs.gpu.as_deref().unwrap_or("").as_bytes());
        h.update([0]);
        for c in capabilities {
            h.update(c.as_bytes());
            h.update([0]);
        }
        h.update([1]); // terminator between the capability list and the ts
        h.update(created_ts.to_le_bytes());
        // Folded in ONLY when present, so every record signed before mTLS —
        // and every pre-mTLS client still signing without it — verifies
        // against the exact preimage bytes it produced.
        if let Some(fp) = tls_fp {
            h.update([2]);
            h.update(fp.as_bytes());
        }
        h.finalize().to_vec()
    }

    /// Mint a signed profile heartbeat. `tls_fp` is the SHA-256 fingerprint of
    /// this agent's TLS certificate, when transport pinning is enabled.
    pub fn create(
        id_key: &Identity,
        name: impl Into<String>,
        specs: MachineSpecs,
        capabilities: Vec<String>,
        created_ts: i64,
        tls_fp: Option<String>,
    ) -> Self {
        let name = name.into();
        let preimage = Self::preimage(&name, &specs, &capabilities, created_ts, tls_fp.as_deref());
        AgentProfile {
            sig: id_key.sign_hex(&preimage),
            agent: id_key.id(),
            name,
            specs,
            capabilities,
            created_ts,
            tls_fp,
        }
    }

    /// Verify the signature is authentic for the stated agent + content.
    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(
            &self.name,
            &self.specs,
            &self.capabilities,
            self.created_ts,
            self.tls_fp.as_deref(),
        );
        verify_hex(&self.agent, &preimage, &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    fn specs() -> MachineSpecs {
        MachineSpecs {
            os: "macos".into(),
            arch: "aarch64".into(),
            cpus: 12,
            ram_mb: 65536,
            gpu: Some("Apple M3 Max".into()),
        }
    }

    #[test]
    fn profile_roundtrips_and_verifies() {
        let k = id();
        let p = AgentProfile::create(&k, "Mordecai the Gravecaller", specs(), vec!["coder".into(), "reproduce".into()], 1000, None);
        assert!(p.verify());
        assert_eq!(p.agent, k.id());
        assert_eq!(p.specs.cpus, 12);
    }

    #[test]
    fn tamper_breaks_verification() {
        let k = id();
        let mut p = AgentProfile::create(&k, "n", specs(), vec!["coder".into()], 1000, None);
        p.specs.ram_mb = 1; // downgrade the advertised RAM after signing
        assert!(!p.verify());
        let mut q = AgentProfile::create(&k, "n", specs(), vec!["coder".into()], 1000, None);
        q.capabilities.push("forged".into());
        assert!(!q.verify());
    }

    #[test]
    fn tls_pin_is_signed_and_tamper_evident() {
        let k = id();
        let p = AgentProfile::create(&k, "n", specs(), vec![], 1000, Some("aa".repeat(32)));
        assert!(p.verify());
        // Swapping the pinned fingerprint after signing must fail — otherwise a
        // MITM could re-point the wire pin without the identity key.
        let mut swapped = p.clone();
        swapped.tls_fp = Some("bb".repeat(32));
        assert!(!swapped.verify());
        // Stripping the pin entirely must also fail (downgrade resistance).
        let mut stripped = p.clone();
        stripped.tls_fp = None;
        assert!(!stripped.verify());
    }
}
