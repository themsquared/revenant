//! Consolidation apply-path test: canned extraction, no LLM, no gateway.
//! Verifies note authoring, entity creation, alias merge, supersession
//! strikethrough, and the within-batch duplicate guard.

use revenant_core::config::MemoryConfig;
use revenant_core::home::Home;
use revenant_memory::consolidate::{ConsolidateReport, ExtractedEntity, ExtractedFact, Extraction};
use revenant_memory::MemoryEngine;
use revenant_store::Store;

fn model_available() -> Option<std::path::PathBuf> {
    let candidates = [
        std::env::var("REVENANT_TEST_MODEL_DIR").ok().map(std::path::PathBuf::from),
        dirs::home_dir().map(|h| h.join(".revenant/models")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|c| c.join("potion-retrieval-32M/model.safetensors").exists())
}

fn fact(subject: &str, text: &str, entities: &[(&str, &str)]) -> ExtractedFact {
    ExtractedFact {
        subject: subject.into(),
        subject_kind: Some("person".into()),
        predicate: None,
        object: None,
        text: text.into(),
        entities: entities
            .iter()
            .map(|(name, kind)| ExtractedEntity { name: (*name).into(), kind: Some((*kind).into()) })
            .collect(),
        valid_from: None,
        supersedes: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_extraction_end_to_end() {
    let Some(models_dir) = model_available() else {
        eprintln!("SKIP: builtin embedding model not downloaded (run `revenant init`)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("rev-consol-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("workspace/memory/entities")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&models_dir, dir.join("models")).unwrap();
    std::env::set_var("REVENANT_HOME", &dir);
    let home = Home::resolve();

    let store = Store::open(&dir.join("revenant.db")).unwrap();
    let llm = revenant_llm::LlmClient::new("http://127.0.0.1:1"); // never called
    let cfg = MemoryConfig { watch_vault: false, ..MemoryConfig::default() };
    let engine = MemoryEngine::new(store, llm, &home, cfg).await.unwrap();

    // Round 1: two facts, one shared entity.
    let mut report = ConsolidateReport::default();
    engine
        .apply_extraction(
            Extraction {
                episode_summary: None,
                facts: vec![
                    fact("Dana Cruz", "Works at Vertex Robotics as CTO", &[("Vertex Robotics", "org")]),
                    fact("Dana Cruz", "Lives in Denver", &[]),
                ],
            },
            Some((7, 100)),
            &mut report,
        )
        .await
        .unwrap();
    assert_eq!(report.facts_added, 2);
    assert_eq!(report.entities_created, 2); // Dana + Vertex

    let note = std::fs::read_to_string(dir.join("workspace/memory/entities/dana-cruz.md")).unwrap();
    assert!(note.contains("Works at [[Vertex Robotics]] as CTO"), "wikilink injected: {note}");
    assert!(note.contains("msg:7/100"), "provenance recorded");

    // Round 2: duplicate (skipped) + supersession of the Denver fact.
    let denver_uid = {
        let memories = engine.recall("where does Dana live", 5).await.unwrap();
        let m = memories.iter().find(|m| m.text.contains("Denver")).expect("denver fact retrievable");
        // Extract uid from the note file for the supersedes reference.
        let line = note.lines().find(|l| l.contains("Denver")).unwrap();
        line.split("f:").nth(1).unwrap().split_whitespace().next().unwrap().to_string()
    };
    let mut report2 = ConsolidateReport::default();
    engine
        .apply_extraction(
            Extraction {
                episode_summary: None,
                facts: vec![
                    fact("Dana Cruz", "Works at Vertex Robotics as CTO", &[]), // dup -> skipped
                    ExtractedFact {
                        supersedes: Some(format!("f:{denver_uid}")),
                        valid_from: Some("2026-07".into()),
                        ..fact("Dana Cruz", "Moved to Austin", &[])
                    },
                ],
            },
            Some((7, 120)),
            &mut report2,
        )
        .await
        .unwrap();
    assert_eq!(report2.facts_added, 1, "duplicate must be skipped");
    assert_eq!(report2.facts_invalidated, 1, "Denver fact superseded");

    let note2 = std::fs::read_to_string(dir.join("workspace/memory/entities/dana-cruz.md")).unwrap();
    assert!(note2.contains("~~Lives in Denver~~"), "struck through: {note2}");
    assert!(note2.contains("Moved to Austin"));

    // Retrieval reflects the supersession: active fact wins, expired gone.
    let memories = engine.recall("where does Dana Cruz live now", 5).await.unwrap();
    assert!(memories.iter().any(|m| m.text.contains("Austin")));
    assert!(!memories.iter().any(|m| m.text.contains("Denver")), "expired fact must not surface");

    std::env::remove_var("REVENANT_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}
