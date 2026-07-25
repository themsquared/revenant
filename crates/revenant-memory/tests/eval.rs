//! Retrieval accuracy + latency gate for M1.5a.
//!
//! Builds a fixture vault (25 entities, ~120 facts), reindexes, then asks 20
//! questions asserting the expected fact lands in the top-5 (hit@5 >= 18/20,
//! MRR >= 0.82) and that recall latency stays under budget. `multi_hop_retrieval`
//! additionally gates the graph leg on questions whose answers are 2-3 relation
//! hops from the entity named in the question.
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

/// Build the fixture home + vault (10 themed entities + 15 synthetic = 25)
/// shared by both eval tests. Returns the temp home dir. `suffix` keeps the
/// two tests' temp dirs from colliding if they ever run concurrently.
///
/// This is a straight extraction of the fixture-setup that used to live
/// inline in `retrieval_accuracy_and_latency` — the emitted vault is
/// byte-identical, so that test's behavior and assertions are unchanged.
fn build_fixture(models_dir: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rev-eval-{}-{suffix}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("workspace/memory/entities")).unwrap();
    std::fs::create_dir_all(dir.join("workspace/memory/episodes")).unwrap();
    // Symlink the real model into the test home.
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(models_dir, dir.join("models")).unwrap();

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
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn retrieval_accuracy_and_latency() {
    let Some(models_dir) = model_available() else {
        eprintln!("SKIP: builtin embedding model not downloaded (run `revenant init`)");
        return;
    };

    let dir = build_fixture(&models_dir, "single");
    std::env::set_var("REVENANT_HOME", &dir);
    let home = Home::resolve();

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
    // MRR gate is 0.82, not the original 0.7 — and the graph leg is the reason.
    // Measured by ablation (graph_leg() forced to return no candidates,
    // everything else untouched): MRR falls 0.860 -> 0.704 and hit@5 20/20 ->
    // 19/20. The original 0.7 gate therefore passed with the graph leg ENTIRELY
    // DEAD, by a margin of 0.004 — it could not detect losing a whole retrieval
    // leg. This eval, not the multi-hop one, is where the graph leg's ranking
    // contribution shows up, so this is the assertion that has to protect it.
    //
    // The scoring inputs are deterministic (static embeddings, BM25, and
    // fixed-iteration PPR over a fixed fixture) and MRR measures 0.860 on every
    // run observed. It is NOT deterministic by construction, though: RRF fuses
    // into a HashMap and equal-scoring items are ordered by hash iteration, so
    // ties can reorder between runs — observed while ablating, where MRR moved
    // between 0.704 and 0.729. Hence a gate with real headroom (0.04) rather
    // than one pinned to the observed value.
    //
    // 0.860 (not 0.885) is the current figure because the per-entity fact cap
    // that made multi-hop 9/9 costs a little single-hop ranking precision —
    // hit@5 is unchanged at 20/20, so the right fact is still always in the
    // top-5, just sometimes a slot lower. That trade is deliberate and
    // documented in `multi_hop_retrieval`. Raise this gate only after
    // re-measuring; lower it only with a documented reason.
    assert!(mrr >= 0.82, "MRR {mrr:.3} below gate (0.82) — retrieval quality regressed");
    assert!(
        p50 < std::time::Duration::from_millis(25),
        "p50 {p50:?} over 25ms budget"
    );

    std::env::remove_var("REVENANT_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Multi-hop retrieval: exercises the graph leg (Personalized PageRank over
/// the entity neighborhood, `src/graph.rs`) specifically. Every question
/// here is phrased around one entity, but its answer lives in a fact
/// belonging to a DIFFERENT entity two or more relation-hops away via the
/// `## Relations` edges in the fixture — so no single fact contains the
/// full answer, and a pure BM25+cosine retriever keyed on the question's
/// surface terms has no lexical/semantic reason to surface it. Only graph
/// traversal (seed the named entity -> walk relation edges -> surface the
/// connected entity's facts) should reach it.
///
/// `retrieval_accuracy_and_latency` (above) never tests this — all 20 of
/// its questions are answerable from a single fact directly, so it could
/// never tell you whether the graph leg contributes anything at all. Per
/// graph-engineering's rule: "if the graph doesn't win on multi-hop
/// questions, it isn't earning its maintenance cost" — this test measures
/// that instead of assuming it.
///
/// K=10 (not top-5) because multi-hop is strictly harder: the answer fact
/// competes for ranking against the seed entity's own, more lexically/
/// semantically similar facts, so we give the graph leg's contribution
/// room to land inside a realistic injection window.
///
/// Note: `revenant_memory::retrieve` (which defines `LEG_GRAPH`) is a
/// private module, not reachable from this external test crate. `Memory`
/// and its public `legs: u8` bitmask ARE public (see `src/lib.rs`), and the
/// bit values (FTS=1, VEC=2, GRAPH=4) are part of that public contract via
/// `Memory.legs`, so we mirror the constant locally rather than reaching
/// into a private module.
const LEG_GRAPH: u8 = 4;

#[tokio::test(flavor = "multi_thread")]
async fn multi_hop_retrieval() {
    let Some(models_dir) = model_available() else {
        eprintln!("SKIP: builtin embedding model not downloaded (run `revenant init`)");
        return;
    };

    let dir = build_fixture(&models_dir, "multihop");
    std::env::set_var("REVENANT_HOME", &dir);
    let home = Home::resolve();

    let store = Store::open(&dir.join("revenant.db")).unwrap();
    let llm = revenant_llm::LlmClient::new("http://127.0.0.1:1"); // never called
    let engine = MemoryEngine::new(store, llm, &home, MemoryConfig::default())
        .await
        .expect("engine init + reindex");
    let status = engine.status().await.unwrap();
    assert_eq!(status.entities, 25, "all entities indexed");

    // Each question names one entity; the expected substring is a fact that
    // belongs to a DIFFERENT entity, reached only via the relation chain
    // noted in the comment.
    const K: usize = 10;
    let questions: Vec<(&str, &str)> = vec![
        // Jane -manages-> Orion Project -> "Depends on ClickHouse..."
        ("What database does the project Jane manages depend on?", "ClickHouse"),
        // Alex Chen -manages-> Orion Project -> "built in Go"
        ("What language is the project that Alex Chen manages written in?", "built in Go"),
        // Orion Project <-owned_by- ... -manages- Jane/Alex -> works_at -> Nimbus Labs
        ("Where does the person who manages Orion Project work?", "Nimbus Labs"),
        // Nimbus Labs <-works_at- Alex Chen -> Spot -> "golden retriever"
        ("What breed is the pet belonging to the person who works at Nimbus Labs?", "golden retriever"),
        // Orion Project -owned_by-> Nimbus Labs -> "Headquartered in Seattle"
        ("Where is the company that owns the Orion Project headquartered?", "Seattle"),
        // Spot -belongs_to-> Alex Chen -> "Lives in Portland Oregon"
        ("What city does Spot's owner live in?", "Portland"),
        // Homelab -belongs_to-> Alex Chen -manages-> Orion Project -> "Ships quarterly releases"
        ("How often does the project managed by the homelab's owner ship releases?", "quarterly releases"),
        // Marathon Training -owned_by-> Alex Chen -> "Allergic to peanuts"
        ("What is the marathon trainee allergic to?", "peanuts"),
        // Nimbus Labs <-works_at- Jane Rivera -> "Joined Nimbus Labs in 2023"
        ("What year did the Orion Project's engineering manager join the company?", "2023"),
    ];

    let mut hits = 0usize;
    let mut graph_credited = 0usize;
    for (question, expected) in &questions {
        let memories = engine.recall(question, K).await.unwrap();
        let hit = memories.iter().find(|m| m.text.contains(expected));
        match hit {
            Some(m) => {
                hits += 1;
                if m.legs & LEG_GRAPH != 0 {
                    graph_credited += 1;
                }
            }
            None => {
                eprintln!("MISS: {question:?} — expected {expected:?}");
                for m in &memories {
                    eprintln!("   got: {} (legs={})", m.text, m.legs);
                }
            }
        }
    }

    eprintln!(
        "multi-hop eval: hit@{K} = {hits}/{}, graph-leg credited on {graph_credited}/{hits} hits",
        questions.len()
    );

    // OBSERVED (run locally, `cargo test -p revenant-memory --test eval --
    // --nocapture` with REVENANT_TEST_MODEL_DIR set to a real
    // potion-retrieval-32M download): hit@10 = 9/9, graph-leg credited on
    // 9/9. Every multi-hop answer carries the LEG_GRAPH bit, and the graph
    // leg is doing work a pure BM25+cosine retriever structurally cannot
    // (there is no lexical/semantic overlap between e.g. "What database does
    // the project Jane manages depend on?" and the fact "Depends on
    // ClickHouse for metrics storage" living on a different entity's note).
    //
    // This was 8/9 when the test was written. The last miss was the one
    // 3-hop chain ("What breed is the pet belonging to the person who works
    // at Nimbus Labs?" -> Nimbus Labs -> Alex Chen -> Spot -> "golden
    // retriever"), and it turned out NOT to be an α-decay limit as originally
    // supposed: the graph leg was emitting facts in strict entity-rank order,
    // so the ~14 facts belonging to the three nearest entities consumed the
    // whole window before Spot could appear at all. A per-entity fact cap
    // (MAX_FACTS_PER_ENTITY in retrieve.rs) fixed it. Measured cost of that
    // cap: single-hop MRR 0.885 -> 0.860 with hit@5 unchanged at 20/20, i.e.
    // the correct fact is still always in the top-5, occasionally one slot
    // lower. See also the negative result recorded in graph.rs — steering the
    // traversal by reweighting typed edges was tried here and did nothing.
    //
    // Bar set at 9/9 (the observed number) rather than padding it —
    // tighten further only once more multi-hop cases are added and
    // re-measured; loosen it only with a documented reason (e.g. an
    // intentional PPR parameter change), never to silently hide a
    // regression.
    const MULTI_HOP_HIT_BAR: usize = 9;
    assert!(
        hits >= MULTI_HOP_HIT_BAR,
        "multi-hop hit@{K} {hits}/{} below gate ({MULTI_HOP_HIT_BAR}) — graph leg may not be pulling its weight",
        questions.len()
    );
    // Gate `graph_credited` too, not just `hits`. Ablation (graph_leg() forced
    // to return no candidates) now gives hit@10 8/9 with graph_credited 0/8, so
    // both assertions fire — but they fail for different reasons and both are
    // worth keeping. `hits` catches "the answers stopped being reachable";
    // `graph_credited` catches "another leg is carrying them instead", which is
    // the failure mode that hid here originally: BM25+cosine independently reach
    // several of these facts (note the `legs=7` = FTS|VEC|GRAPH values in the
    // MISS dump above), so before the per-entity cap `hits` was 8/9 with OR
    // without the graph leg and only this assertion could tell the difference.
    //
    // Read it precisely: it proves the graph leg RETRIEVES these multi-hop
    // answers, NOT that it is the only leg that can. The honest measure of
    // necessity is the differential — see the MRR gate in
    // `retrieval_accuracy_and_latency`.
    assert!(
        graph_credited >= MULTI_HOP_HIT_BAR,
        "graph leg credited on only {graph_credited}/{hits} multi-hop hits (gate {MULTI_HOP_HIT_BAR}) \
         — the graph leg has stopped reaching these answers even if other legs still do"
    );

    std::env::remove_var("REVENANT_HOME");
    let _ = std::fs::remove_dir_all(&dir);
}
