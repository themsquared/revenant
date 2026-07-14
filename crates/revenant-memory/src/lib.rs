//! revenant-memory: graph-native, Obsidian-compatible agent memory.
//!
//! The markdown vault is the source of truth; SQLite holds a rebuildable
//! derived index (entities, bi-temporal facts/edges, FTS, embeddings).
//! Retrieval is hybrid (BM25 + cosine + personalized PageRank, RRF-fused)
//! with zero LLM calls. Consolidation (LLM extraction) runs off the hot
//! path and lands in M1.5b.

pub mod consolidate;
pub mod embed;
pub mod graph;
pub mod index;
mod retrieve;
pub mod vault;
mod watch;

pub use consolidate::ConsolidateReport;

use anyhow::{Context, Result};
use embed::{BuiltinEmbedder, Embedder, GatewayEmbedder};
use revenant_core::config::{EmbedderKind, MemoryConfig};
use revenant_core::home::Home;
use revenant_store::Store;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use vault::{norm_name, Note, Vault};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ItemKey {
    Fact(String),
    Message(i64),
    Note(String),
    Entity(i64),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Memory {
    pub text: String,
    pub note: Option<String>,
    pub note_path: Option<String>,
    pub score: f32,
    /// Which legs found it (FTS=1, VEC=2, GRAPH=4) — debuggability.
    pub legs: u8,
    pub provenance: Option<Provenance>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Provenance {
    pub session_id: Option<i64>,
    pub message_id: Option<i64>,
    pub valid_from: Option<i64>,
    pub invalid_at: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Episode {
    pub session_id: i64,
    pub user_message_id: i64,
    pub assistant_message_id: i64,
    pub user_text: String,
    pub assistant_text: String,
    pub at: i64,
}

#[derive(Debug, Default)]
pub struct EmbedCache {
    pub items: Vec<(ItemKey, Vec<f32>)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryStatus {
    pub entities: i64,
    pub facts: i64,
    pub edges: i64,
    pub pending: i64,
    pub embedder: String,
    pub vault: String,
}

pub struct MemoryEngine {
    pub(crate) store: Store,
    pub(crate) llm: revenant_llm::LlmClient,
    pub(crate) cfg: MemoryConfig,
    pub(crate) vault: Vault,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) embed_cache: RwLock<EmbedCache>,
    /// (normalized name or alias, entity id) — graph seeding.
    pub(crate) entity_names: RwLock<Vec<(String, i64)>>,
    /// fact uid -> subject entity id — graph leg fact ranking.
    pub(crate) fact_subjects: RwLock<HashMap<String, Option<i64>>>,
    /// note_path -> display title.
    pub(crate) note_titles: RwLock<HashMap<String, String>>,
    /// Wakes the background consolidator when an episode arrives.
    pub(crate) wakeup: Arc<tokio::sync::Notify>,
    /// Self-event suppression for the vault watcher: rel path -> content hash
    /// of our own most recent write.
    pub(crate) suppress: std::sync::Mutex<HashMap<String, [u8; 32]>>,
}

impl MemoryEngine {
    pub async fn new(
        store: Store,
        llm: revenant_llm::LlmClient,
        home: &Home,
        cfg: MemoryConfig,
    ) -> Result<Arc<Self>> {
        let vault_root = cfg
            .vault_path
            .clone()
            .unwrap_or_else(|| home.memory_dir());
        std::fs::create_dir_all(vault_root.join("entities"))?;
        std::fs::create_dir_all(vault_root.join("episodes"))?;

        let embedder: Arc<dyn Embedder> = match cfg.embedder {
            EmbedderKind::Builtin => Arc::new(
                BuiltinEmbedder::load(&home.models_dir().join(embed::BUILTIN_MODEL))
                    .context("loading builtin embedding model")?,
            ),
            EmbedderKind::Gateway => {
                let model = cfg
                    .embed_model
                    .clone()
                    .context("[memory] embedder = 'gateway' requires embed_model")?;
                Arc::new(GatewayEmbedder::new(llm.clone(), model))
            }
        };

        let engine = Arc::new(MemoryEngine {
            store,
            llm,
            cfg,
            vault: Vault::new(vault_root),
            embedder,
            embed_cache: RwLock::new(EmbedCache::default()),
            entity_names: RwLock::new(Vec::new()),
            fact_subjects: RwLock::new(HashMap::new()),
            note_titles: RwLock::new(HashMap::new()),
            wakeup: Arc::new(tokio::sync::Notify::new()),
            suppress: std::sync::Mutex::new(HashMap::new()),
        });

        // Embedding-model versioning: on mismatch, rebuild the index rather
        // than serve stale-model vectors (the OpenClaw failure mode).
        let recorded = index::meta_get(&engine.store, "embed_model").await?;
        match recorded.as_deref() {
            Some(id) if id == engine.embedder.id() => {
                engine.warm_caches().await?;
            }
            Some(other) => {
                tracing::warn!(
                    "embedding model changed ({other} -> {}) — reindexing vault",
                    engine.embedder.id()
                );
                engine.reindex().await?;
            }
            None => {
                engine.reindex().await?;
            }
        }
        Ok(engine)
    }

    pub fn cfg(&self) -> &MemoryConfig {
        &self.cfg
    }

    pub fn vault_root(&self) -> &std::path::Path {
        self.vault.root()
    }

    /// Embed arbitrary texts with this agent's embedder — so callers (e.g. the
    /// codex tool) can semantically rank content that isn't in the vault.
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embedder.embed(texts)
    }

    /// Rebuild ALL derived state (SQLite) from the markdown vault.
    pub async fn reindex(&self) -> Result<MemoryStatus> {
        index::wipe_derived(&self.store).await?;

        let notes_raw = self.vault.walk()?;
        let mut parsed: Vec<(String, Note, bool)> = Vec::new(); // (path, note, dirty)
        for (path, raw) in &notes_raw {
            match Note::parse(raw) {
                Ok(note) => parsed.push((path.clone(), note, false)),
                Err(err) => tracing::warn!("skipping unparseable note {path}: {err:#}"),
            }
        }

        // Pass 1: entities (entity notes AND episode notes become graph nodes).
        let mut ids_by_path: HashMap<String, i64> = HashMap::new();
        let mut ids_by_norm: HashMap<String, i64> = HashMap::new();
        for (path, note, _) in &parsed {
            let title = if note.title.is_empty() {
                path.trim_end_matches(".md")
                    .rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .to_string()
            } else {
                note.title.clone()
            };
            let text_for_embedding = format!("{title}\n{}", note.body_pre);
            let embedding = self
                .embedder
                .embed_one(&text_for_embedding)
                .ok()
                .map(|v| embed::vec_to_blob(&v));
            let entity_id = index::upsert_entity(
                &self.store,
                index::NewEntity {
                    uid: note.front.uid.clone(),
                    name: title.clone(),
                    norm_name: norm_name(&title),
                    kind: if note.front.kind.is_empty() {
                        "concept".into()
                    } else {
                        note.front.kind.clone()
                    },
                    note_path: path.clone(),
                    summary: note.body_pre.lines().next().map(String::from),
                    embedding,
                },
            )
            .await?;
            ids_by_path.insert(path.clone(), entity_id);
            ids_by_norm.insert(norm_name(&title), entity_id);
            for alias in &note.front.aliases {
                let norm = norm_name(alias);
                index::add_alias(&self.store, &norm, entity_id).await?;
                ids_by_norm.entry(norm).or_insert(entity_id);
            }
            // Episode/entity body text is FTS-searchable.
            if !note.body_pre.is_empty() {
                self.store
                    .recall_index("vault", path, &format!("{title}\n{}", note.body_pre))
                    .await?;
            }
        }

        // Pass 2: facts + edges.
        let now = unix_now();
        for (path, note, dirty) in &mut parsed {
            let subject_id = ids_by_path.get(path.as_str()).copied();

            // Prose wikilinks -> weak "mentions" edges.
            if let Some(src) = subject_id {
                for link in vault::wikilinks(&note.body_pre) {
                    if let Some(&dst) = ids_by_norm.get(&norm_name(&link)) {
                        if dst != src {
                            index::insert_edge(&self.store, src, dst, "mentions", 0.5, None)
                                .await?;
                        }
                    }
                }
            }

            for fact in &mut note.facts {
                let uid = match &fact.uid {
                    Some(uid) => uid.clone(),
                    None => {
                        // Human-authored bullet: adopt it (assign uid, mark
                        // the note for a one-time rewrite).
                        let uid = short_uid("f");
                        fact.uid = Some(uid.clone());
                        *dirty = true;
                        uid
                    }
                };
                let embedding = self
                    .embedder
                    .embed_one(&fact.text)
                    .ok()
                    .map(|v| embed::vec_to_blob(&v));
                let fact_id = index::insert_fact(
                    &self.store,
                    index::NewFact {
                        uid: uid.clone(),
                        note_path: path.clone(),
                        subject_id,
                        predicate: None,
                        object: None,
                        text: fact.text.clone(),
                        embedding,
                        valid_from: fact.valid_from.as_deref().and_then(parse_date),
                        invalid_at: fact.valid_until.as_deref().and_then(parse_date),
                        expired_at: if fact.struck { Some(now) } else { None },
                        source_session_id: fact.msg.map(|(s, _)| s),
                        source_message_id: fact.msg.map(|(_, m)| m),
                    },
                )
                .await?;
                if !fact.struck {
                    self.store.recall_index("fact", &uid, &fact.text).await?;
                    // Fact wikilinks -> mention edges carrying the fact.
                    if let Some(src) = subject_id {
                        for link in &fact.links {
                            if let Some(&dst) = ids_by_norm.get(&norm_name(link)) {
                                if dst != src {
                                    index::insert_edge(
                                        &self.store,
                                        src,
                                        dst,
                                        "mentions",
                                        0.5,
                                        Some(fact_id),
                                    )
                                    .await?;
                                }
                            }
                        }
                    }
                }
            }

            // Typed relations.
            if let Some(src) = subject_id {
                for rel in &note.relations {
                    if let Some(&dst) = ids_by_norm.get(&norm_name(&rel.target)) {
                        if dst != src {
                            index::insert_edge(&self.store, src, dst, &rel.rel, 1.0, None).await?;
                        }
                    } else {
                        tracing::debug!("unresolved relation target [[{}]] in {path}", rel.target);
                    }
                }
            }
        }

        // One-time rewrite of notes that gained fact uids.
        for (path, note, dirty) in &parsed {
            if *dirty {
                self.write_note(path, &note.render())?;
            }
        }

        index::meta_set(&self.store, "embed_model", &self.embedder.id()).await?;
        self.warm_caches().await?;
        self.status().await
    }

    /// Load caches (embeddings, entity names, fact subjects, note titles)
    /// from the index.
    async fn warm_caches(&self) -> Result<()> {
        let entities = index::entities_all(&self.store).await?;
        let facts = index::facts_active(&self.store).await?;

        let mut items = Vec::with_capacity(entities.len() + facts.len());
        let mut names = Vec::with_capacity(entities.len());
        let mut titles = HashMap::new();
        for entity in &entities {
            if let Some(blob) = &entity.embedding {
                items.push((ItemKey::Entity(entity.id), embed::blob_to_vec(blob)));
            }
            names.push((entity.norm_name.clone(), entity.id));
            titles.insert(entity.note_path.clone(), entity.name.clone());
        }
        // Aliases seed the name list too.
        let aliases: Vec<(String, i64)> = self
            .store
            .with(|conn| {
                let mut stmt = conn.prepare("SELECT alias_norm, entity_id FROM mem_aliases")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect()
            })
            .await?;
        names.extend(aliases);

        let mut subjects = HashMap::with_capacity(facts.len());
        for fact in &facts {
            if let Some(blob) = &fact.embedding {
                items.push((ItemKey::Fact(fact.uid.clone()), embed::blob_to_vec(blob)));
            }
            subjects.insert(fact.uid.clone(), fact.subject_id);
        }

        *self.embed_cache.write().unwrap() = EmbedCache { items };
        *self.entity_names.write().unwrap() = names;
        *self.fact_subjects.write().unwrap() = subjects;
        *self.note_titles.write().unwrap() = titles;
        Ok(())
    }

    /// Explicit save (memory_save tool): write an episodic note now, index
    /// it, queue consolidation. No LLM on this path.
    pub async fn save(&self, content: &str, session_id: i64) -> Result<String> {
        let date = fmt_date_full(unix_now());
        let hint: String = content.split_whitespace().take(5).collect::<Vec<_>>().join(" ");
        let path = self.vault.episode_path(&date, &hint);
        let rel = format!(
            "episodes/{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let uid = short_uid("p");
        let mut note = Note::new(&uid, "episode", &format!("{date} — {hint}"));
        note.body_pre = content.to_string();
        note.front
            .extra
            .insert("session".into(), serde_yaml::Value::from(session_id));
        self.write_note(&rel, &note.render())?;

        // Index immediately (entity node + FTS + embedding).
        let embedding = self
            .embedder
            .embed_one(&format!("{} {content}", note.title))
            .ok()
            .map(|v| embed::vec_to_blob(&v));
        let entity_id = index::upsert_entity(
            &self.store,
            index::NewEntity {
                uid,
                name: note.title.clone(),
                norm_name: norm_name(&note.title),
                kind: "episode".into(),
                note_path: rel.clone(),
                summary: content.lines().next().map(String::from),
                embedding: embedding.clone(),
            },
        )
        .await?;
        self.store
            .recall_index("vault", &rel, &format!("{}\n{content}", note.title))
            .await?;
        if let Some(blob) = embedding {
            self.embed_cache
                .write()
                .unwrap()
                .items
                .push((ItemKey::Entity(entity_id), embed::blob_to_vec(&blob)));
        }
        self.note_titles
            .write()
            .unwrap()
            .insert(rel.clone(), note.title.clone());

        index::pending_push(
            &self.store,
            "explicit_save",
            &serde_json::json!({ "note_path": rel, "session_id": session_id }).to_string(),
        )
        .await?;
        Ok(rel)
    }

    /// Non-blocking: queue a finished turn for consolidation and wake the
    /// background pass (debounced).
    pub fn observe(&self, episode: Episode) {
        let store = self.store.clone();
        let wakeup = self.wakeup.clone();
        let payload = serde_json::to_string(&episode).unwrap_or_default();
        tokio::spawn(async move {
            if let Err(err) = index::pending_push(&store, "episode", &payload).await {
                tracing::warn!("failed to queue episode for consolidation: {err:#}");
            } else {
                wakeup.notify_one();
            }
        });
    }

    /// Suppression-aware atomic note write: the vault watcher will ignore
    /// the event our own write generates.
    pub(crate) fn write_note(&self, rel: &str, content: &str) -> Result<()> {
        let hash = self.vault.write_atomic(rel, content)?;
        self.suppress.lock().unwrap().insert(rel.to_string(), hash);
        Ok(())
    }

    pub(crate) async fn warm_caches_public(&self) -> Result<()> {
        self.warm_caches().await
    }

    /// Background work: debounced consolidation on new episodes + periodic
    /// sweep + vault watcher. Call once from the daemon.
    pub fn start_background(self: &Arc<Self>) {
        let engine = self.clone();
        tokio::spawn(async move {
            let sweep = std::time::Duration::from_secs(engine.cfg.sweep_interval_s.max(60));
            let debounce = std::time::Duration::from_secs(engine.cfg.consolidate_debounce_s);
            loop {
                tokio::select! {
                    _ = engine.wakeup.notified() => {
                        // Debounce: let a burst of turns settle into one batch.
                        tokio::time::sleep(debounce).await;
                    }
                    _ = tokio::time::sleep(sweep) => {}
                }
                match engine.consolidate_now().await {
                    Ok(report) if report.episodes_processed > 0 => {
                        tracing::info!(
                            "memory consolidated: {} episodes -> {} facts (+{} entities, {} invalidated)",
                            report.episodes_processed,
                            report.facts_added,
                            report.entities_created,
                            report.facts_invalidated
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!("consolidation pass failed: {err:#}"),
                }
            }
        });
        if self.cfg.watch_vault {
            watch::start(self.clone());
        }
    }

    pub async fn status(&self) -> Result<MemoryStatus> {
        let (entities, facts, edges, pending) = index::counts(&self.store).await?;
        Ok(MemoryStatus {
            entities,
            facts,
            edges,
            pending,
            embedder: self.embedder.id(),
            vault: self.vault.root().display().to_string(),
        })
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn short_uid(prefix: &str) -> String {
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &uuid[..8])
}

/// "YYYY", "YYYY-MM", or "YYYY-MM-DD" -> unix epoch (start of period).
fn parse_date(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next().map(|p| p.parse().ok()).unwrap_or(Some(1))?;
    let day: i64 = parts.next().map(|p| p.parse().ok()).unwrap_or(Some(1))?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400)
}

fn fmt_date_full(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parse_round_trip() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2024-01"), parse_date("2024-01-01"));
        assert_eq!(parse_date("2024"), parse_date("2024-01-01"));
        assert_eq!(fmt_date_full(parse_date("2026-07-09").unwrap()), "2026-07-09");
        assert_eq!(parse_date("not-a-date"), None);
    }
}
