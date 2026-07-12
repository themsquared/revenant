//! Retrieval accuracy + latency gate for M1.5a.
//!
//! Builds a fixture vault (25 entities, ~120 facts), reindexes, then asks 20
//! questions asserting the expected fact lands in the top-5 (hit@5 >= 18/20,
//! MRR >= 0.7) and that recall latency stays under budget.
//!
//! Requires the builtin embedding model; if it isn't downloaded (CI without
//! `revenant init`), the test SKIPS with a notice rather than failing.

use revenant_core::config::MemoryConfig;
use revenant_core::home::Home;
use revenant_memory::MemoryEngine;
use revenant_store::Store;

fn model_available() -> Option<std::path::PathBuf> {
    // Prefer an explicit override, else the user's real download.
    let candidates = [
        std::env::var("REVENANT_TEST_MODEL_DIR").ok().map(std::path::PathBuf::from),
        dirs::home_dir().map(|h| h.join(".revenant/models")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| candidate.join("potion-retrieval-32M/model.safetensors").exists())
}

fn entity_note(uid: &str, kind: &str, title: &str, facts: &[&str], relations: &[(&str, &str)]) -> String {
    let mut out = format!("---\nuid: {uid}\nkind: {kind}\ntags: [{kind}]\n---\n\n# {title}\n");
    if !facts.is_empty() {
        out.push_str("\n## Facts\n");
        for (i, fact) in facts.iter().enumerate() {
            out.push_str(&format!("- {fact} <!-- f:{uid}-{i} -->\n"));
        }
    }
    if !relations.is_empty() {
        out.push_str("\n## Relations\n");
        for (rel, target) in relations {
            out.push_str(&format!("- {rel} [[{target}]]\n"));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn retrieval_accuracy_and_latency() {
    let Some(models_dir) = model_available() else {
        eprintln!("SKIP: builtin embedding model not downloaded (run `revenant init`)");
        return;
    };

    let dir = std::env::temp_dir().join(format!("rev-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("workspace/memory/entities")).unwrap();
    std::fs::create_dir_all(dir.join("workspace/memory/episodes")).unwrap();
    // Symlink the real model into the test home.
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&models_dir, dir.join("models")).unwrap();
    std::env::set_var("REVENANT_HOME", &dir);
    let home = Home::resolve();

    // ---- fixture vault: 10 themed entities + 15 synthetic = 25 ----
    let vault = dir.join("workspace/memory/entities");
    type ThemedEntity<'a> = (&'a str, &'a str, &'a str, Vec<&'a str>, Vec<(&'a str, &'a str)>);
    let themed: Vec<ThemedEntity> = vec![
        ("e-owner", "person", "Alex Chen",
         vec!["Works at Nimbus Labs as a platform engineer",
              "Allergic to peanuts",
              "Lives in Portland Oregon",
              "Prefers communicating over Signal",
              "Birthday is March 12"],
         vec![("works_at", "Nimbus Labs"), ("manages", "Orion Project")]),
        ("e-nimbus", "org", "Nimbus Labs",
         vec!["Cloud infrastructure startup with 40 employees",
              "Headquartered in Seattle",
              "Runs everything on Kubernetes"],
         vec![]),
        ("e-orion", "project", "Orion Project",
         vec!["Internal observability platform built in Go",
              "Ships quarterly releases",
              "Depends on ClickHouse for metrics storage"],
         vec![("owned_by", "Nimbus Labs")]),
        ("e-jane", "person", "Jane Rivera",
         vec!["Engineering manager for the Orion Project",
              "Joined Nimbus Labs in 2023",
              "Strong opinions about code review latency"],
         vec![("works_at", "Nimbus Labs"), ("manages", "Orion Project")]),
        ("e-spot", "thing", "Spot",
         vec!["Alex Chen's golden retriever",
              "Needs medication every morning",
              "Afraid of thunderstorms"],
         vec![("belongs_to", "Alex Chen")]),
        ("e-homelab", "thing", "Homelab",
         vec!["Three Raspberry Pi 5 nodes running k3s",
              "Hosts a Jellyfin media server",
              "Backed up nightly to Backblaze"],
         vec![("belongs_to", "Alex Chen")]),
        ("e-marathon", "project", "Marathon Training",
         vec!["Training for the Portland Marathon in October",
              "Long runs happen on Saturday mornings",
              "Current weekly mileage is 35 miles"],
         vec![("owned_by", "Alex Chen")]),
        ("e-carla", "person", "Carla Nguyen",
         vec!["Alex Chen's accountant",
              "Prefers documents as PDF attachments",
              "Files quarterly taxes in the first week of the quarter"],
         vec![]),
        ("e-cabin", "place", "Hood River Cabin",
         vec!["Family cabin two hours from Portland",
              "Has terrible cell coverage but good wifi",
              "Booked for the second week of August"],
         vec![("belongs_to", "Alex Chen")]),
        ("e-bikeshop", "org", "Cascade Cycles",
         vec!["Local bike shop that services Alex Chen's gravel bike",
              "Closed on Mondays"],
         vec![]),
    ];
    for (uid, kind, title, facts, relations) in &themed {
        let content = entity_note(uid, kind, title, facts, relations);
        let slug = title.to_lowercase().replace([' ', '.'], "-");
        std::fs::write(vault.join(format!("{slug}.md")), content).unwrap();
    }
    // Synthetic filler: 15 entities x 5 facts = 75 (noise the retriever must ignore).
    for i in 0..15 {
        let title = format!("Vendor {i}");
        let facts: Vec<String> = (0..5)
            .map(|j| format!("Provides service package {j} under contract {i}{j}"))
            .collect();
        let fact_refs: Vec<&str> = facts.iter().map(String::as_str).collect();
        let content = entity_note(&format!("e-syn{i:02}"), "org", &title, &fact_refs, &[]);
        std::fs::write(vault.join(format!("vendor-{i}.md")), content).unwrap();
    }

    // ---- engine ----
    let store = Store::open(&dir.join("revenant.db")).unwrap();
    let llm = revenant_llm::LlmClient::new("http://127.0.0.1:1"); // never called
    let engine = MemoryEngine::new(store, llm, &home, MemoryConfig::default())
        .await
        .expect("engine init + reindex");
    let status = engine.status().await.unwrap();
    assert_eq!(status.entities, 25, "all entities indexed");
    assert!(status.facts >= 100, "facts indexed, got {}", status.facts);

    // ---- 20 questions -> substring expected in a top-5 fact ----
    let questions: Vec<(&str, &str)> = vec![
        ("Where does Alex work?", "Nimbus Labs as a platform engineer"),
        ("What is Alex allergic to?", "peanuts"),
        ("What city does Alex live in?", "Portland"),
        ("How should I contact Alex?", "Signal"),
        ("When is Alex's birthday?", "March 12"),
        ("Where is Nimbus Labs headquartered?", "Seattle"),
        ("How many employees does Nimbus Labs have?", "40 employees"),
        ("What is the Orion Project written in?", "built in Go"),
        ("What database does Orion use for metrics?", "ClickHouse"),
        ("Who manages the Orion Project?", "Engineering manager for the Orion"),
        ("When did Jane join the company?", "2023"),
        ("What kind of dog is Spot?", "golden retriever"),
        ("What is Spot afraid of?", "thunderstorms"),
        ("What runs on the homelab?", "k3s"),
        ("Where is the homelab backed up?", "Backblaze"),
        ("Which marathon is Alex training for?", "Portland Marathon"),
        ("What is Alex's weekly running mileage?", "35 miles"),
        ("How does Carla want documents sent?", "PDF attachments"),
        ("When is the cabin booked?", "second week of August"),
        ("What day is the bike shop closed?", "Mondays"),
    ];

    let mut hits_at_5 = 0usize;
    let mut mrr = 0.0f64;
    let mut latencies = Vec::new();
    for (question, expected) in &questions {
        let start = std::time::Instant::now();
        let memories = engine.recall(question, 5).await.unwrap();
        latencies.push(start.elapsed());
        let rank = memories.iter().position(|m| m.text.contains(expected));
        match rank {
            Some(r) => {
                hits_at_5 += 1;
                mrr += 1.0 / (r as f64 + 1.0);
            }
            None => {
                eprintln!("MISS: {question:?} — expected {expected:?}");
                for m in &memories {
                    eprintln!("   got: {}", m.text);
                }
            }
        }
    }
    mrr /= questions.len() as f64;

    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    eprintln!("eval: hit@5 = {hits_at_5}/20, MRR = {mrr:.3}, p50 = {p50:.2?}");

    assert!(hits_at_5 >= 18, "hit@5 {hits_at_5}/20 below gate (18)");
    assert!(mrr >= 0.7, "MRR {mrr:.3} below gate (0.7)");
    assert!(
        p50 < std::time::Duration::from_millis(25),
        "p50 {p50:?} over 25ms budget"
    );

    std::env::remove_var("REVENANT_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}
