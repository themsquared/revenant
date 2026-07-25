//! The private horde board — a coordination queue for ONE account's own agents.
//!
//! This is deliberately *not* the public quest board. Quests live in the
//! network economy (bounties, reputation, and the hard no-self-dealing rule
//! that forbids solving your own quests). The horde board is the opposite: it
//! is account-private work, and only agents of the *same* account may take it.
//! No credits change hands, no reputation is earned — it is purely your own
//! revenants dividing a job among themselves (distributed thinking).
//!
//! Same signing discipline as the rest of the protocol: every record is a
//! content-addressed sha256 preimage + Ed25519 signature, byte-identical on the
//! agent and the server, so a receiver trusts the key, not the directory.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A unit of account-private work, posted by the orchestrating agent for one of
/// the account's own agents to pick up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeTask {
    /// Content address: sha256 of the signing preimage, lowercase hex.
    pub id: String,
    /// The run this task belongs to — the orchestrator groups subtasks under a
    /// single run id so results can be gathered and synthesized together.
    pub run: String,
    /// Signer — the orchestrating agent; its account scopes the whole board.
    pub author: String,
    pub title: String,
    /// What to do (the subtask prompt).
    pub spec: String,
    /// Capability hints — a worker prefers tasks whose sigils it advertises.
    #[serde(default)]
    pub sigils: Vec<String>,
    pub created_ts: i64,
    pub sig: String,
}

/// Domain tag: binds a signature to this record type and no other.
const DOMAIN_TASK: &[u8] = b"rev-horde-task-v1";
const DOMAIN_CLAIM: &[u8] = b"rev-horde-claim-v1";
const DOMAIN_RESULT: &[u8] = b"rev-horde-result-v1";

impl HordeTask {
    fn preimage(
        domain: Option<&[u8]>,
        run: &str,
        title: &str,
        spec: &str,
        sigils: &[String],
        created_ts: i64,
    ) -> Vec<u8> {
        let mut h = crate::identity::preimage_hasher(domain);
        h.update(run.as_bytes());
        h.update([0]);
        h.update(title.as_bytes());
        h.update([0]);
        h.update(spec.as_bytes());
        h.update([0]);
        for s in sigils {
            h.update(s.as_bytes());
            h.update([0]);
        }
        h.update([1]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(
        id_key: &Identity,
        run: impl Into<String>,
        title: impl Into<String>,
        spec: impl Into<String>,
        sigils: Vec<String>,
        created_ts: i64,
    ) -> Self {
        let (run, title, spec) = (run.into(), title.into(), spec.into());
        let preimage = Self::preimage(Some(DOMAIN_TASK), &run, &title, &spec, &sigils, created_ts);
        HordeTask {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            run,
            title,
            spec,
            sigils,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        // Domain-tagged first; untagged accepted only for records signed before
        // domain separation existed (see identity::preimage_hasher).
        [Some(DOMAIN_TASK), None].into_iter().any(|domain| {
            let preimage =
                Self::preimage(domain, &self.run, &self.title, &self.spec, &self.sigils, self.created_ts);
            hex::encode(Sha256::digest(&preimage)) == self.id
                && verify_hex(&self.author, &preimage, &self.sig)
        })
    }
}

/// A worker's signed claim on a horde task — holds a short lease so two of the
/// account's agents don't both grind the same subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeClaim {
    pub id: String,
    pub task: String,
    pub worker: String,
    pub created_ts: i64,
    pub sig: String,
}

impl HordeClaim {
    fn preimage(domain: Option<&[u8]>, task: &str, created_ts: i64) -> Vec<u8> {
        let mut h = crate::identity::preimage_hasher(domain);
        h.update(task.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(id_key: &Identity, task: impl Into<String>, created_ts: i64) -> Self {
        let task = task.into();
        let preimage = Self::preimage(Some(DOMAIN_CLAIM), &task, created_ts);
        HordeClaim {
            id: hex::encode(Sha256::digest(&preimage)),
            worker: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            task,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        [Some(DOMAIN_CLAIM), None].into_iter().any(|domain| {
            let preimage = Self::preimage(domain, &self.task, self.created_ts);
            hex::encode(Sha256::digest(&preimage)) == self.id
                && verify_hex(&self.worker, &preimage, &self.sig)
        })
    }
}

/// A worker's signed result for a horde task — the subtask's answer, bound to
/// its content so it can't be swapped and to the task so it can't be replayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeResult {
    pub id: String,
    pub task: String,
    pub worker: String,
    pub output: String,
    pub created_ts: i64,
    pub sig: String,
}

impl HordeResult {
    fn preimage(domain: Option<&[u8]>, task: &str, output: &str, created_ts: i64) -> Vec<u8> {
        let mut h = crate::identity::preimage_hasher(domain);
        h.update(task.as_bytes());
        h.update([0]);
        h.update(output.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(
        id_key: &Identity,
        task: impl Into<String>,
        output: impl Into<String>,
        created_ts: i64,
    ) -> Self {
        let (task, output) = (task.into(), output.into());
        let preimage = Self::preimage(Some(DOMAIN_RESULT), &task, &output, created_ts);
        HordeResult {
            id: hex::encode(Sha256::digest(&preimage)),
            worker: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            task,
            output,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        [Some(DOMAIN_RESULT), None].into_iter().any(|domain| {
            let preimage = Self::preimage(domain, &self.task, &self.output, self.created_ts);
            hex::encode(Sha256::digest(&preimage)) == self.id
                && verify_hex(&self.worker, &preimage, &self.sig)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    #[test]
    fn task_roundtrips_and_binds_run() {
        let a = id();
        let t = HordeTask::create(&a, "run-1", "shard 0", "sum 0..100", vec!["compute".into()], 1000);
        assert!(t.verify());
        assert_eq!(t.author, a.id());
        assert_eq!(t.run, "run-1");
        let mut bad = t.clone();
        bad.spec = "sum 0..999999".into(); // change the work after signing
        assert!(!bad.verify());
        let mut bad2 = t.clone();
        bad2.run = "run-2".into(); // can't move a task to another run
        assert!(!bad2.verify());
    }

    #[test]
    fn claim_and_result_bind_their_task() {
        let w = id();
        let c = HordeClaim::create(&w, "task-abc", 5);
        assert!(c.verify());
        assert_eq!(c.worker, w.id());
        let r = HordeResult::create(&w, "task-abc", "answer=42", 7);
        assert!(r.verify());
        let mut tampered = r.clone();
        tampered.output = "answer=1".into();
        assert!(!tampered.verify());
        let mut replayed = r.clone();
        replayed.task = "task-xyz".into();
        assert!(!replayed.verify());
    }
}

#[cfg(test)]
mod domain_confusion_tests {
    use super::*;
    use crate::identity::Identity;

    /// RED-TEAM: one signature, two record types.
    ///
    /// `HordeClaim` hashes `task \0 ts` and `HordeResult` hashes
    /// `task \0 output \0 ts`. Neither preimage names its own type, so a
    /// Result for task `T` with an empty output produces the SAME bytes as a
    /// Claim for task `T\0` — and a signature over one therefore verifies as
    /// the other. This test documents the weakness domain separation fixes.
    #[test]
    fn a_result_signature_verifies_as_a_claim() {
        let key = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let ts = 1_784_000_000i64;

        // A legitimately signed result with an empty output.
        let result = HordeResult::create(&key, "abc123", "", ts);
        assert!(result.verify(), "baseline: the result is authentic");

        // Re-present that exact signature as a CLAIM, task id + one NUL.
        let forged = HordeClaim {
            id: result.id.clone(),
            worker: result.worker.clone(),
            task: "abc123\0".to_string(),
            created_ts: ts,
            sig: result.sig.clone(),
        };
        assert!(
            !forged.verify(),
            "a result's signature must NOT verify as a claim — preimages are not domain-separated"
        );
    }

    /// The compatibility guarantee that makes this change safe to deploy: every
    /// record signed BEFORE domain separation must still verify, because the
    /// Necropolis ledger re-verifies its whole history on replay (`Dir::apply`)
    /// and a rejected historical entry would break the chain.
    #[test]
    fn legacy_untagged_records_still_verify() {
        let key = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let ts = 1_780_000_000i64;

        // Hand-build a record exactly as a pre-domain-separation agent would:
        // untagged preimage, id and sig derived from it.
        let legacy_preimage = HordeClaim::preimage(None, "task-legacy", ts);
        let legacy = HordeClaim {
            id: hex::encode(Sha256::digest(&legacy_preimage)),
            worker: key.id(),
            task: "task-legacy".to_string(),
            created_ts: ts,
            sig: key.sign_hex(&legacy_preimage),
        };
        assert!(legacy.verify(), "pre-domain-tag records must keep verifying");

        // And tampering with a legacy record is still caught.
        let mut tampered = legacy.clone();
        tampered.task = "task-other".to_string();
        assert!(!tampered.verify(), "legacy path must not become a forgery oracle");
    }

    /// RESIDUAL EXPOSURE — and, unlike A2A, it is PERMANENT rather than
    /// transitional. This was originally filed as "drop the `None` arm once every
    /// agent has upgraded". Every agent HAS upgraded, and it still cannot be
    /// dropped here.
    ///
    /// Why: horde records are persisted in the Necropolis ledger and re-verified
    /// on every replay (`Dir::apply` — `if t.verify()` for horde_task/claim/
    /// result). Removing the untagged arm would make ~2400 historical records
    /// fail verification, and `apply` SKIPS what fails — so history would not
    /// break loudly, it would silently disappear from rebuilt state. That is
    /// worse than the exposure it closes.
    ///
    /// So the window stays open for pre-tag signatures on ledger-backed types.
    /// Practical bound on the exposure: forging this way needs a legacy
    /// signature, whose `created_ts` is inside the preimage and therefore cannot
    /// be moved forward, and claims/results are lease- and account-gated
    /// downstream.
    ///
    /// Closing it properly needs an ERA MARKER on ledger entries so replay can
    /// select the verifier that entry was written with — a protocol change, and
    /// the honest next step rather than deleting the arm. A2A envelopes had no
    /// such constraint (never persisted, never replayed) and phase 2 IS closed
    /// there; see a2a.rs.
    #[test]
    fn ledger_backed_types_keep_the_legacy_window_by_necessity() {
        let key = Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap();
        let ts = 1_780_000_000i64;

        // A legacy-signed result with empty output...
        let p = HordeResult::preimage(None, "abc123", "", ts);
        let sig = key.sign_hex(&p);

        // ...still verifies as a legacy claim on "abc123\0". This asserts the
        // KNOWN residual hole, so if a future change closes it this test fails
        // loudly and gets deleted along with the `None` arms.
        let forged = HordeClaim {
            id: hex::encode(Sha256::digest(&p)),
            worker: key.id(),
            task: "abc123\0".to_string(),
            created_ts: ts,
            sig,
        };
        assert!(
            forged.verify(),
            "if this now FAILS the legacy arm was removed — check that ledger \
             replay still accepts pre-tag records before keeping that change"
        );
    }
}
