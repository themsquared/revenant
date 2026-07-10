//! Consolidation: the write path's expensive half, OFF the hot path.
//!
//! Episodes queue in `mem_pending` (durable — survives crashes). A pass
//! drains a batch, makes ONE fast-tier extraction call (forced tool =
//! structured output), resolves entities through the cheap-first ladder,
//! applies bi-temporal invalidation for contradictions, and updates
//! markdown notes atomically. The LLM never sits between the user and a
//! reply.

use crate::index;
use crate::vault::{norm_name, Note};
use crate::{Episode, MemoryEngine};
use anyhow::{Context, Result};
use revenant_llm::{MessagesRequest, WireMessage};
use serde::Deserialize;
use serde_json::json;

/// Extraction tier alias — cheap, structured-output-capable.
const EXTRACTION_TIER: &str = "fast";
/// Ladder thresholds.
const JW_AUTO_MERGE: f64 = 0.92;
const JW_GRAY_BAND: f64 = 0.85;
const COSINE_AUTO_MERGE: f32 = 0.90;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ConsolidateReport {
    pub episodes_processed: usize,
    pub facts_added: usize,
    pub facts_invalidated: usize,
    pub entities_created: usize,
    pub entities_merged: usize,
    pub gray_band_queued: usize,
}

#[derive(Debug, Deserialize)]
pub struct Extraction {
    #[serde(default)]
    pub episode_summary: Option<String>,
    #[serde(default)]
    pub facts: Vec<ExtractedFact>,
}

#[derive(Debug, Deserialize)]
pub struct ExtractedFact {
    pub subject: String,
    #[serde(default)]
    pub subject_kind: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    pub text: String,
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Existing fact uid this one contradicts/replaces.
    #[serde(default)]
    pub supersedes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
}

const EXTRACTOR_SYSTEM: &str = "You extract durable memories from conversation transcripts. \
Record ONLY facts worth remembering long-term: preferences, biography, relationships, projects, \
decisions, commitments, recurring events. Skip chit-chat, transient state, questions, and \
anything already present in KNOWN FACTS — unless the transcript contradicts a known fact, in \
which case record the new fact and set `supersedes` to the old fact's id. Subjects and entities \
should be proper names where possible. `text` must read as a standalone bullet about the subject. \
Return via the record_memory tool. An empty facts array is a fine answer.";

fn record_memory_spec() -> revenant_core::ToolSpec {
    revenant_core::ToolSpec {
        name: "record_memory".into(),
        description: "Record extracted durable memories.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "episode_summary": {"type": ["string", "null"], "description": "1-2 sentence summary if the conversation is worth an episode note, else null"},
                "facts": {"type": "array", "items": {"type": "object", "properties": {
                    "subject": {"type": "string"},
                    "subject_kind": {"type": "string", "enum": ["person","org","place","project","thing","concept"]},
                    "predicate": {"type": "string", "description": "snake_case verb phrase, e.g. works_at"},
                    "object": {"type": "string"},
                    "text": {"type": "string", "description": "standalone bullet, may use [[Entity]] wikilinks"},
                    "entities": {"type": "array", "items": {"type": "object", "properties": {
                        "name": {"type": "string"}, "kind": {"type": "string"}}, "required": ["name"]}},
                    "valid_from": {"type": ["string", "null"], "description": "YYYY-MM or YYYY-MM-DD if stated"},
                    "supersedes": {"type": ["string", "null"], "description": "uid of a KNOWN FACT this replaces"}
                }, "required": ["subject", "text"]}}
            },
            "required": ["facts"]
        }),
    }
}

enum Resolution {
    Existing(i64),
    Merged(i64),
    Created(i64),
    GrayBand(i64),
}

impl MemoryEngine {
    /// Drain pending episodes now. Called by the background sweep, the CLI,
    /// and tests.
    pub async fn consolidate_now(&self) -> Result<ConsolidateReport> {
        let mut report = ConsolidateReport::default();
        loop {
            let batch =
                index::pending_list(&self.store, "episode", self.cfg.consolidate_batch).await?;
            if batch.is_empty() {
                break;
            }
            let ids: Vec<i64> = batch.iter().map(|row| row.id).collect();
            let episodes: Vec<Episode> = batch
                .iter()
                .filter_map(|row| serde_json::from_str(&row.payload).ok())
                .collect();
            if episodes.is_empty() {
                index::pending_delete(&self.store, ids).await?;
                continue;
            }

            match self.extract(&episodes).await {
                Ok(extraction) => {
                    let episode_meta = episodes.last().map(|e| (e.session_id, e.assistant_message_id));
                    self.apply_extraction(extraction, episode_meta, &mut report).await?;
                    report.episodes_processed += episodes.len();
                    index::pending_delete(&self.store, ids).await?;
                }
                Err(err) => {
                    tracing::warn!("extraction failed (will retry): {err:#}");
                    index::pending_bump_attempts(&self.store, ids).await?;
                    break; // don't hammer a failing gateway
                }
            }
        }
        if report.facts_added > 0 || report.entities_created > 0 || report.facts_invalidated > 0 {
            self.refresh_caches().await?;
        }
        Ok(report)
    }

    /// ONE fast-tier structured-output call for a batch of episodes.
    async fn extract(&self, episodes: &[Episode]) -> Result<Extraction> {
        // Context: known entities (name + uid) and active facts for entities
        // name-matched in the episode text — lets the model dedupe/supersede.
        let entities = index::entities_all(&self.store).await?;
        let mut known_entities = String::new();
        for entity in entities.iter().take(150) {
            known_entities.push_str(&format!("- {} (kind {})\n", entity.name, entity.kind));
        }
        let episode_text: String = episodes
            .iter()
            .map(|e| format!("[user msg {}]: {}\n[assistant]: {}\n", e.user_message_id, e.user_text, e.assistant_text))
            .collect();
        let episode_norm = norm_name(&episode_text);
        let mentioned: Vec<i64> = entities
            .iter()
            .filter(|e| {
                e.norm_name
                    .split(' ')
                    .any(|tok| tok.len() >= 4 && episode_norm.contains(tok))
            })
            .map(|e| e.id)
            .collect();
        let facts = index::facts_active(&self.store).await?;
        let mut known_facts = String::new();
        for fact in facts
            .iter()
            .filter(|f| f.subject_id.map(|s| mentioned.contains(&s)).unwrap_or(false))
            .take(60)
        {
            known_facts.push_str(&format!("- [{}] {}\n", fact.uid, fact.text));
        }

        let user_content = format!(
            "KNOWN ENTITIES:\n{}\nKNOWN FACTS (id in brackets):\n{}\nTRANSCRIPT:\n{}",
            if known_entities.is_empty() { "(none)\n" } else { &known_entities },
            if known_facts.is_empty() { "(none)\n" } else { &known_facts },
            episode_text
        );

        let request = MessagesRequest {
            model: EXTRACTION_TIER.to_string(),
            max_tokens: 2048,
            system: Some(serde_json::Value::String(EXTRACTOR_SYSTEM.to_string())),
            messages: vec![WireMessage::new(
                revenant_core::Role::User,
                vec![revenant_core::ContentBlock::text(user_content)],
            )],
            tools: vec![record_memory_spec()],
            tool_choice: Some(json!({"type": "tool", "name": "record_memory"})),
            stream: true,
        };
        let outcome = self.llm.stream_message(&request, |_| {}).await?;
        let input = outcome
            .content
            .iter()
            .find_map(|block| match block {
                revenant_core::ContentBlock::ToolUse { name, input, .. }
                    if name == "record_memory" =>
                {
                    Some(input.clone())
                }
                _ => None,
            })
            .context("extractor did not call record_memory")?;
        Ok(serde_json::from_value(input)?)
    }

    /// Apply an extraction: ladder-resolve entities, write facts + notes,
    /// invalidate superseded facts. Pure of LLM calls — unit-testable.
    pub async fn apply_extraction(
        &self,
        mut extraction: Extraction,
        episode_meta: Option<(i64, i64)>,
        report: &mut ConsolidateReport,
    ) -> Result<()> {
        // Models return junk like "<UNKNOWN>" instead of null — keep only
        // dates that actually parse.
        for fact in &mut extraction.facts {
            fact.valid_from = fact
                .valid_from
                .take()
                .filter(|s| crate::parse_date(s).is_some());
            fact.supersedes = fact.supersedes.take().filter(|s| !s.contains('<'));
        }
        for fact in &extraction.facts {
            // 1. Resolve the subject entity (creating its note if new).
            let subject_id = match self
                .resolve_entity(&fact.subject, fact.subject_kind.as_deref(), report)
                .await?
            {
                Resolution::Existing(id)
                | Resolution::Merged(id)
                | Resolution::Created(id)
                | Resolution::GrayBand(id) => id,
            };

            // 2. Resolve secondary entities (object + mentions).
            let mut linked_ids = Vec::new();
            for entity in &fact.entities {
                if norm_name(&entity.name) == norm_name(&fact.subject) {
                    continue;
                }
                let (Resolution::Existing(id)
                | Resolution::Merged(id)
                | Resolution::Created(id)
                | Resolution::GrayBand(id)) = self
                    .resolve_entity(&entity.name, entity.kind.as_deref(), report)
                    .await?;
                linked_ids.push((entity.name.clone(), id));
            }

            // 3. Supersession -> bi-temporal invalidation + strikethrough.
            if let Some(old_uid) = fact.supersedes.as_deref().map(clean_uid) {
                let invalid_at = fact.valid_from.as_deref().and_then(crate::parse_date);
                if index::expire_fact(&self.store, &old_uid, invalid_at).await? {
                    report.facts_invalidated += 1;
                    self.strike_in_note(&old_uid, fact.valid_from.as_deref()).await?;
                }
            }

            // 4. Duplicate guard: skip facts that near-match an active fact
            // of the same subject (the extractor's known-facts context only
            // covers pre-batch facts, so within-batch dupes land here).
            let existing = index::facts_active(&self.store).await?;
            let fact_vec = self.embedder.embed_one(&fact.text).ok();
            let is_duplicate = existing.iter().any(|row| {
                if row.subject_id != Some(subject_id) {
                    return false;
                }
                if norm_name(&row.text) == norm_name(&fact.text) {
                    return true;
                }
                match (&fact_vec, &row.embedding) {
                    (Some(qv), Some(blob)) => {
                        crate::embed::cosine(qv, &crate::embed::blob_to_vec(blob)) >= 0.95
                    }
                    _ => false,
                }
            });
            if is_duplicate {
                tracing::debug!("skipping near-duplicate fact: {}", fact.text);
                continue;
            }

            // 5. Append the fact to the subject's note + index it.
            let uid = crate::short_uid("f");
            let note_path = self.entity_note_path(subject_id).await?;
            let mut fact_text = fact.text.clone();
            // Ensure linked entities appear as wikilinks (Obsidian graph).
            for (name, _) in &linked_ids {
                if fact_text.contains(name.as_str()) && !fact_text.contains(&format!("[[{name}"))
                {
                    fact_text = fact_text.replace(name.as_str(), &format!("[[{name}]]"));
                }
            }
            let fact_line = crate::vault::FactLine {
                uid: Some(uid.clone()),
                text: fact_text.clone(),
                links: crate::vault::wikilinks(&fact_text),
                valid_from: fact.valid_from.clone(),
                valid_until: None,
                msg: episode_meta,
                struck: false,
            };
            self.append_fact_to_note(&note_path, fact_line).await?;

            let embedding = self
                .embedder
                .embed_one(&fact_text)
                .ok()
                .map(|v| crate::embed::vec_to_blob(&v));
            let fact_id = index::insert_fact(
                &self.store,
                index::NewFact {
                    uid: uid.clone(),
                    note_path: note_path.clone(),
                    subject_id: Some(subject_id),
                    predicate: fact.predicate.clone(),
                    object: fact.object.clone(),
                    text: fact_text.clone(),
                    embedding,
                    valid_from: fact.valid_from.as_deref().and_then(crate::parse_date),
                    invalid_at: None,
                    expired_at: None,
                    source_session_id: episode_meta.map(|(s, _)| s),
                    source_message_id: episode_meta.map(|(_, m)| m),
                },
            )
            .await?;
            self.store.recall_index("fact", &uid, &fact_text).await?;
            report.facts_added += 1;

            // 6. Edges: typed predicate edge to the object entity, weak
            // mention edges to the rest.
            for (name, dst) in &linked_ids {
                if dst == &subject_id {
                    continue;
                }
                let is_object = fact
                    .object
                    .as_deref()
                    .map(|o| norm_name(o) == norm_name(name))
                    .unwrap_or(false);
                let (rel, weight) = if is_object {
                    (fact.predicate.clone().unwrap_or_else(|| "related_to".into()), 1.0)
                } else {
                    ("mentions".to_string(), 0.5)
                };
                index::insert_edge(&self.store, subject_id, *dst, &rel, weight, Some(fact_id))
                    .await?;
            }
        }

        // Episode note, if the extractor thought it noteworthy.
        if let Some(summary) = extraction
            .episode_summary
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if let Some((session_id, _)) = episode_meta {
                let _ = self.save(summary, session_id).await?;
            }
        }
        // New facts/entities must be immediately retrievable.
        self.refresh_caches().await?;
        Ok(())
    }

    /// Entity-resolution ladder: exact norm -> alias -> Jaro-Winkler +
    /// cosine auto-merge -> gray band (provisional entity + queued
    /// adjudication) -> create.
    async fn resolve_entity(
        &self,
        name: &str,
        kind: Option<&str>,
        report: &mut ConsolidateReport,
    ) -> Result<Resolution> {
        let norm = norm_name(name);
        let entities = index::entities_all(&self.store).await?;

        // Rung 1+2: exact norm or alias.
        if let Some(entity) = entities.iter().find(|e| e.norm_name == norm) {
            return Ok(Resolution::Existing(entity.id));
        }
        let alias_hit: Option<i64> = {
            let names = self.entity_names.read().unwrap();
            names.iter().find(|(n, _)| *n == norm).map(|(_, id)| *id)
        };
        if let Some(id) = alias_hit {
            return Ok(Resolution::Existing(id));
        }

        // Rung 3: fuzzy + embedding similarity.
        let name_vec = self.embedder.embed_one(name).ok();
        let mut best: Option<(&index::EntityRow, f64, f32)> = None;
        for entity in &entities {
            let jw = strsim::jaro_winkler(&norm, &entity.norm_name);
            if jw < JW_GRAY_BAND {
                continue;
            }
            let cos = match (&name_vec, &entity.embedding) {
                (Some(qv), Some(blob)) => {
                    crate::embed::cosine(qv, &crate::embed::blob_to_vec(blob))
                }
                _ => 0.0,
            };
            if best.map(|(_, bjw, _)| jw > bjw).unwrap_or(true) {
                best = Some((entity, jw, cos));
            }
        }
        if let Some((entity, jw, cos)) = best {
            if jw >= JW_AUTO_MERGE && cos >= COSINE_AUTO_MERGE {
                index::add_alias(&self.store, &norm, entity.id).await?;
                self.entity_names.write().unwrap().push((norm, entity.id));
                report.entities_merged += 1;
                return Ok(Resolution::Merged(entity.id));
            }
            // Gray band: create a provisional entity, queue adjudication.
            let id = self.create_entity(name, kind, report).await?;
            index::pending_push(
                &self.store,
                "merge_adjudication",
                &json!({ "provisional": name, "candidate": entity.name, "jw": jw, "cos": cos })
                    .to_string(),
            )
            .await?;
            report.gray_band_queued += 1;
            return Ok(Resolution::GrayBand(id));
        }

        Ok(Resolution::Created(self.create_entity(name, kind, report).await?))
    }

    async fn create_entity(
        &self,
        name: &str,
        kind: Option<&str>,
        report: &mut ConsolidateReport,
    ) -> Result<i64> {
        let kind = kind.unwrap_or("concept");
        let uid = crate::short_uid("e");
        let rel = format!("entities/{}.md", crate::vault::slug(name));
        let note = Note::new(&uid, kind, name);
        self.write_note(&rel, &note.render())?;

        let embedding = self
            .embedder
            .embed_one(name)
            .ok()
            .map(|v| crate::embed::vec_to_blob(&v));
        let id = index::upsert_entity(
            &self.store,
            index::NewEntity {
                uid,
                name: name.to_string(),
                norm_name: norm_name(name),
                kind: kind.to_string(),
                note_path: rel.clone(),
                summary: None,
                embedding,
            },
        )
        .await?;
        self.entity_names.write().unwrap().push((norm_name(name), id));
        self.note_titles.write().unwrap().insert(rel, name.to_string());
        report.entities_created += 1;
        Ok(id)
    }

    async fn entity_note_path(&self, entity_id: i64) -> Result<String> {
        self.store
            .with(move |conn| {
                conn.query_row(
                    "SELECT note_path FROM mem_entities WHERE id = ?1",
                    [entity_id],
                    |r| r.get(0),
                )
            })
            .await
    }

    async fn append_fact_to_note(&self, rel: &str, fact: crate::vault::FactLine) -> Result<()> {
        let raw = self.vault.read(rel)?;
        let mut note = Note::parse(&raw).with_context(|| format!("parsing {rel}"))?;
        note.facts.push(fact);
        self.write_note(rel, &note.render())?;
        Ok(())
    }

    /// Strike a superseded fact in whichever note holds it.
    async fn strike_in_note(&self, fact_uid: &str, until: Option<&str>) -> Result<()> {
        let rows = index::facts_by_uids(&self.store, vec![fact_uid.to_string()]).await?;
        let Some(row) = rows.first() else { return Ok(()) };
        let raw = self.vault.read(&row.note_path)?;
        let mut note = Note::parse(&raw).with_context(|| format!("parsing {}", row.note_path))?;
        if note.strike_fact(fact_uid, until) {
            self.write_note(&row.note_path, &note.render())?;
        }
        // The struck fact leaves the FTS index (history stays in mem_facts).
        let uid = fact_uid.to_string();
        self.store
            .with(move |conn| {
                conn.execute(
                    "DELETE FROM recall WHERE source = 'fact' AND ref = ?1",
                    [&uid],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Incremental cache refresh after a consolidation pass.
    pub(crate) async fn refresh_caches(&self) -> Result<()> {
        self.warm_caches_public().await
    }
}

/// The model sometimes returns "f:abc123" instead of "abc123".
fn clean_uid(raw: &str) -> String {
    raw.trim().trim_start_matches("f:").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_cleaning() {
        assert_eq!(clean_uid("f:abc123"), "abc123");
        assert_eq!(clean_uid("abc123"), "abc123");
        assert_eq!(clean_uid(" f:x "), "x");
    }
}
