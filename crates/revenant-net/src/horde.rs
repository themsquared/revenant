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

impl HordeTask {
    fn preimage(run: &str, title: &str, spec: &str, sigils: &[String], created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
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
        let preimage = Self::preimage(&run, &title, &spec, &sigils, created_ts);
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
        let preimage = Self::preimage(&self.run, &self.title, &self.spec, &self.sigils, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.author, &preimage, &self.sig)
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
    fn preimage(task: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(task.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(id_key: &Identity, task: impl Into<String>, created_ts: i64) -> Self {
        let task = task.into();
        let preimage = Self::preimage(&task, created_ts);
        HordeClaim {
            id: hex::encode(Sha256::digest(&preimage)),
            worker: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            task,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.task, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.worker, &preimage, &self.sig)
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
    fn preimage(task: &str, output: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
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
        let preimage = Self::preimage(&task, &output, created_ts);
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
        let preimage = Self::preimage(&self.task, &self.output, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.worker, &preimage, &self.sig)
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
