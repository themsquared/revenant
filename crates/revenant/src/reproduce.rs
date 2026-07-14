//! The reproduction runner — network-promotion Phase 2b.
//!
//! Trust in the horde is earned by re-verification, not asserted. Given an
//! `Improvement` artifact (a molt) carrying an embedded eval suite + its
//! author's claimed proof, this revenant pulls it, re-runs the suite against
//! its OWN daemon, and signs a reproduction `Attestation` it posts back to the
//! Necropolis. A quorum of distinct peers each doing this — real compute on
//! independent boxes — is what lets a minor change earn auto-promotion.

use anyhow::{bail, Context, Result};
use revenant_core::home::Home;
use revenant_evals::{run_suite, Suite};
use revenant_net::attest::Attestation;
use revenant_net::{ArtifactKind, Identity, NecropolisClient};
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn cmd_reproduce(artifact_id: String) -> Result<()> {
    let home = Home::resolve();
    let cfg = crate::load_config(&home)?;
    let Some(url) = cfg.network.necropolis_url.clone() else {
        bail!("no network.necropolis_url configured — point it at the horde's Necropolis first");
    };
    let id = Identity::load_or_create(&home.identity_dir())?;
    let necro = NecropolisClient::new(url);

    println!("🜁 reproducing {artifact_id} …");
    let artifact = necro.pull(&artifact_id).await.context("pulling artifact")?;
    if !matches!(artifact.kind, ArtifactKind::Improvement) {
        bail!("only Improvement artifacts carry an eval proof to reproduce (got {:?})", artifact.kind);
    }
    let suite: Suite = serde_json::from_slice(&artifact.payload()?)
        .context("artifact payload is not an embedded eval suite (JSON)")?;

    let local = revenant_client::Client::from_env(&home)
        .context("reproduce needs the local daemon — start it with `revenant up`")?;
    local.health().await.context("local daemon not reachable")?;

    println!("  re-running {} eval task(s) on my own box …", suite.tasks.len());
    let report = run_suite(&local, &suite).await.context("running the eval suite locally")?;
    let (reproduced, detail) = decide(report.passed(), report.outcomes.len(), artifact.eval_proof.as_ref());

    let ts = now_secs();
    let att = Attestation::create(&id, &artifact_id, reproduced, &detail, ts);
    necro.publish_reproduction(&att).await.context("publishing reproduction attestation")?;

    let count = necro.reproductions(&artifact_id).await.map(|v| v.len()).unwrap_or(0);
    println!(
        "  {} — {detail}\n  signed {} · {count} reproduction(s) now on record for this molt",
        if reproduced { "✅ reproduced" } else { "❌ did NOT reproduce" },
        id.fingerprint(),
    );
    Ok(())
}

/// Pure reproduction verdict: a molt is reproduced iff the peer's own run of the
/// suite passes every task (the GradeSpecs encode the expected win). The claim
/// (author's proof) is folded into the human detail, best-effort.
fn decide(pass: usize, total: usize, claim: Option<&serde_json::Value>) -> (bool, String) {
    let reproduced = total > 0 && pass == total;
    let claimed = claim
        .and_then(|c| c.get("passed").and_then(|v| v.as_u64()))
        .map(|k| format!(", author claimed {k}"))
        .unwrap_or_default();
    (reproduced, format!("{pass}/{total} eval tasks pass{claimed}"))
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::decide;
    use serde_json::json;

    #[test]
    fn all_pass_reproduces() {
        let (ok, detail) = decide(3, 3, Some(&json!({"passed": 3})));
        assert!(ok);
        assert!(detail.contains("3/3"));
        assert!(detail.contains("claimed 3"));
    }

    #[test]
    fn any_fail_does_not_reproduce() {
        let (ok, _) = decide(2, 3, None);
        assert!(!ok);
    }

    #[test]
    fn empty_suite_does_not_reproduce() {
        let (ok, _) = decide(0, 0, None);
        assert!(!ok);
    }
}
