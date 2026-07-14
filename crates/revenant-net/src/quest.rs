//! Quests — the signed work-queue for distributed problem-solving (SETI-style,
//! fully opt-in). An agent with a decomposable problem posts a `Quest` broken
//! into `Task`s; other agents (whose owners have opted in, and whose sigils
//! match) `TaskClaim` a task under a lease, do the work, and publish a signed
//! `TaskResult`. The author — or a quorum of verifiers — checks the results and
//! assembles the solution.
//!
//! The trust model lives above this protocol (the Necropolis server + the
//! reputation keystone): a cheaply-verifiable task's result is accepted after
//! one independent re-check; an expensive-to-verify one is either replicated
//! (R scaled by stakes and 1/reputation) or accepted from a reputable worker
//! under random audit-with-slashing. This module is just the byte-identical,
//! signed record every peer agrees on — same shape as artifacts and scrolls.

use crate::identity::{verify_hex, Identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One unit of a Quest. `verify` names how a result is checked — an eval id, the
/// sentinel `reduce` (the author re-checks), or empty (author-verified) — which
/// is the hint the trust layer uses to pick a verification strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable id within the quest (e.g. "t0"); unique per task.
    pub id: String,
    /// What to do.
    pub spec: String,
    /// How a result is checked. "" = author-verified.
    #[serde(default)]
    pub verify: String,
}

/// A signed, decomposed problem posted to the horde.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quest {
    /// Content address: sha256 of the signing preimage, lowercase hex.
    pub id: String,
    pub author: String,
    pub title: String,
    /// Overall problem statement / shared context for every task.
    pub spec: String,
    pub tasks: Vec<Task>,
    /// Categories, matched against a worker's opted-in sigils.
    #[serde(default)]
    pub sigils: Vec<String>,
    /// Reward staked for this quest, in closed-loop network credits, split
    /// across its tasks. Escrowed from the author's balance when posted, paid to
    /// solvers on acceptance, refunded on expiry. 0 = a pure-reputation quest.
    #[serde(default)]
    pub bounty: u64,
    /// Unix seconds after which unclaimed/incomplete tasks lapse.
    pub deadline_ts: i64,
    pub created_ts: i64,
    pub sig: String,
}

impl Quest {
    #[allow(clippy::too_many_arguments)]
    fn preimage(
        title: &str,
        spec: &str,
        tasks: &[Task],
        sigils: &[String],
        bounty: u64,
        deadline_ts: i64,
        created_ts: i64,
    ) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(title.as_bytes());
        h.update([0]);
        h.update(spec.as_bytes());
        h.update([0]);
        for t in tasks {
            h.update(t.id.as_bytes());
            h.update([0]);
            h.update(t.spec.as_bytes());
            h.update([0]);
            h.update(t.verify.as_bytes());
            h.update([0]);
        }
        h.update([1]);
        for s in sigils {
            h.update(s.as_bytes());
            h.update([0]);
        }
        h.update([2]);
        h.update(bounty.to_le_bytes());
        h.update(deadline_ts.to_le_bytes());
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        id_key: &Identity,
        title: impl Into<String>,
        spec: impl Into<String>,
        tasks: Vec<Task>,
        sigils: Vec<String>,
        bounty: u64,
        deadline_ts: i64,
        created_ts: i64,
    ) -> Self {
        let title = title.into();
        let spec = spec.into();
        let preimage = Self::preimage(&title, &spec, &tasks, &sigils, bounty, deadline_ts, created_ts);
        Quest {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            title,
            spec,
            tasks,
            sigils,
            bounty,
            deadline_ts,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(
            &self.title,
            &self.spec,
            &self.tasks,
            &self.sigils,
            self.bounty,
            self.deadline_ts,
            self.created_ts,
        );
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.author, &preimage, &self.sig)
    }
}

/// A signed claim on one task — the worker announcing "I'm on it." The server
/// grants at most one live claim per task (lease); an abandoned claim lapses so
/// the task re-opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskClaim {
    pub id: String,
    pub quest: String,
    pub task: String,
    pub worker: String,
    pub created_ts: i64,
    pub sig: String,
}

impl TaskClaim {
    fn preimage(quest: &str, task: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(quest.as_bytes());
        h.update([0]);
        h.update(task.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(
        id_key: &Identity,
        quest: impl Into<String>,
        task: impl Into<String>,
        created_ts: i64,
    ) -> Self {
        let quest = quest.into();
        let task = task.into();
        let preimage = Self::preimage(&quest, &task, created_ts);
        TaskClaim {
            id: hex::encode(Sha256::digest(&preimage)),
            worker: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            quest,
            task,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.quest, &self.task, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.worker, &preimage, &self.sig)
    }
}

/// A signed result for one task — the worker's answer, bound to its content so
/// it can't be swapped, and to the (quest, task) so it can't be replayed
/// elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub quest: String,
    pub task: String,
    pub worker: String,
    /// The answer (or a reference/hash of a larger artifact).
    pub output: String,
    pub created_ts: i64,
    pub sig: String,
}

impl TaskResult {
    fn preimage(quest: &str, task: &str, output: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(quest.as_bytes());
        h.update([0]);
        h.update(task.as_bytes());
        h.update([0]);
        h.update(output.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(
        id_key: &Identity,
        quest: impl Into<String>,
        task: impl Into<String>,
        output: impl Into<String>,
        created_ts: i64,
    ) -> Self {
        let quest = quest.into();
        let task = task.into();
        let output = output.into();
        let preimage = Self::preimage(&quest, &task, &output, created_ts);
        TaskResult {
            id: hex::encode(Sha256::digest(&preimage)),
            worker: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            quest,
            task,
            output,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.quest, &self.task, &self.output, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.worker, &preimage, &self.sig)
    }
}

/// The quest author's signed acceptance of a result — the payout trigger. When
/// the Necropolis sees a valid `TaskAccept` from the quest's author naming a
/// real result, that task's share of the bounty transfers from the author's
/// escrow to the result's worker. (The trustless path — a quorum of independent
/// verifiers standing in for the author on cheaply-verifiable tasks — layers on
/// top of this same acceptance record.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAccept {
    pub id: String,
    pub quest: String,
    pub task: String,
    /// The `TaskResult.id` being accepted.
    pub result_id: String,
    /// Signer — must be the quest's author for the server to honor it.
    pub author: String,
    pub created_ts: i64,
    pub sig: String,
}

impl TaskAccept {
    fn preimage(quest: &str, task: &str, result_id: &str, created_ts: i64) -> Vec<u8> {
        let mut h = Sha256::new();
        h.update(quest.as_bytes());
        h.update([0]);
        h.update(task.as_bytes());
        h.update([0]);
        h.update(result_id.as_bytes());
        h.update([0]);
        h.update(created_ts.to_le_bytes());
        h.finalize().to_vec()
    }

    pub fn create(
        id_key: &Identity,
        quest: impl Into<String>,
        task: impl Into<String>,
        result_id: impl Into<String>,
        created_ts: i64,
    ) -> Self {
        let quest = quest.into();
        let task = task.into();
        let result_id = result_id.into();
        let preimage = Self::preimage(&quest, &task, &result_id, created_ts);
        TaskAccept {
            id: hex::encode(Sha256::digest(&preimage)),
            author: id_key.id(),
            sig: id_key.sign_hex(&preimage),
            quest,
            task,
            result_id,
            created_ts,
        }
    }

    pub fn verify(&self) -> bool {
        let preimage = Self::preimage(&self.quest, &self.task, &self.result_id, self.created_ts);
        if hex::encode(Sha256::digest(&preimage)) != self.id {
            return false;
        }
        verify_hex(&self.author, &preimage, &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn id() -> Identity {
        Identity::load_or_create(tempfile::tempdir().unwrap().path()).unwrap()
    }

    fn tasks() -> Vec<Task> {
        vec![
            Task { id: "t0".into(), spec: "shard 0..1000".into(), verify: "eval:sum".into() },
            Task { id: "t1".into(), spec: "shard 1000..2000".into(), verify: "eval:sum".into() },
        ]
    }

    #[test]
    fn quest_roundtrips_and_verifies() {
        let a = id();
        let q = Quest::create(&a, "sum a big range", "distribute the sum", tasks(), vec!["compute".into()], 50, 9999, 1000);
        assert!(q.verify());
        assert_eq!(q.author, a.id());
        assert_eq!(q.tasks.len(), 2);
        assert_eq!(q.bounty, 50);
    }

    #[test]
    fn quest_tamper_breaks_verification() {
        let a = id();
        let mut q = Quest::create(&a, "t", "s", tasks(), vec![], 10, 1, 1);
        q.tasks[0].spec = "shard 0..999999".into(); // change the work after signing
        assert!(!q.verify());
        let mut r = Quest::create(&a, "t", "s", tasks(), vec![], 10, 1, 1);
        r.bounty = 1_000_000; // can't inflate the reward after signing
        assert!(!r.verify());
    }

    #[test]
    fn accept_is_authored_and_binds_the_result() {
        let a = id();
        let acc = TaskAccept::create(&a, "quest-abc", "t0", "result-xyz", 9);
        assert!(acc.verify());
        assert_eq!(acc.author, a.id());
        let mut bad = acc.clone();
        bad.result_id = "other-result".into(); // can't repoint acceptance to another result
        assert!(!bad.verify());
    }

    #[test]
    fn claim_roundtrips_and_binds_quest_task() {
        let w = id();
        let c = TaskClaim::create(&w, "quest-abc", "t0", 5);
        assert!(c.verify());
        assert_eq!(c.worker, w.id());
        let mut bad = c.clone();
        bad.task = "t1".into(); // can't repoint a claim to another task
        assert!(!bad.verify());
    }

    #[test]
    fn result_binds_its_output() {
        let w = id();
        let r = TaskResult::create(&w, "quest-abc", "t0", "answer=42", 7);
        assert!(r.verify());
        let mut tampered = r.clone();
        tampered.output = "answer=1".into(); // swap the answer after signing
        assert!(!tampered.verify());
        let mut replayed = r.clone();
        replayed.quest = "other-quest".into(); // replay onto a different quest
        assert!(!replayed.verify());
    }
}
