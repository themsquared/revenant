//! Hybrid retrieval: three independent legs (BM25, cosine, graph-PPR) fused
//! with Reciprocal Rank Fusion. Zero LLM calls; budget is single-digit ms.

use crate::embed::cosine;
use crate::graph::personalized_pagerank;
use crate::index;
use crate::{ItemKey, Memory, MemoryEngine, Provenance};
use anyhow::Result;
use std::collections::HashMap;

const RRF_K: f32 = 60.0;
const LEG_TAKE: usize = 32;
/// A result must rank in the top-N of at least one leg to be injectable.
const FLOOR_RANK: usize = 24;
pub const LEG_FTS: u8 = 1;
pub const LEG_VEC: u8 = 2;
pub const LEG_GRAPH: u8 = 4;

impl MemoryEngine {
    pub async fn recall(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let qvec = self.embedder.embed_one(query).ok();

        // Leg A: BM25 over the FTS index (messages + facts + vault bodies).
        let fts_leg: Vec<ItemKey> = self
            .store
            .recall_search(query, LEG_TAKE)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|hit| match hit.source.as_str() {
                "fact" => Some(ItemKey::Fact(hit.reference)),
                "message" => hit.reference.parse().ok().map(ItemKey::Message),
                "vault" => Some(ItemKey::Note(hit.reference)),
                _ => None,
            })
            .collect();

        // Leg B: cosine over the in-RAM embedding cache.
        let (vec_leg, entity_sims, fact_sims) = match &qvec {
            Some(qv) => self.vector_leg(qv),
            None => (vec![], vec![], HashMap::new()),
        };

        // Leg C: seed entities (name/alias match + top cosine entities) →
        // neighborhood → PPR → facts of high-rank entities (cosine
        // tie-break within an entity).
        let graph_leg = self.graph_leg(query, &entity_sims, &fact_sims).await?;

        // RRF fusion.
        let mut fused: HashMap<ItemKey, (f32, u8)> = HashMap::new();
        for (leg, bit) in [(&fts_leg, LEG_FTS), (&vec_leg, LEG_VEC), (&graph_leg, LEG_GRAPH)] {
            for (rank0, key) in leg.iter().enumerate() {
                let entry = fused.entry(key.clone()).or_insert((0.0, 0));
                entry.0 += 1.0 / (RRF_K + (rank0 + 1) as f32);
                entry.1 |= bit;
            }
        }
        let floor = 1.0 / (RRF_K + FLOOR_RANK as f32);
        let mut ranked: Vec<(ItemKey, f32, u8)> = fused
            .into_iter()
            .filter(|(_, (score, _))| *score >= floor)
            .map(|(key, (score, legs))| (key, score, legs))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit.max(limit) * 2); // headroom for hydration losses

        self.hydrate(ranked, limit).await
    }

    /// Cosine leg over cached fact/entity/note embeddings. Also returns
    /// (entity_id, similarity) pairs for graph seeding and a fact-uid
    /// similarity map for graph-leg tie-breaking.
    #[allow(clippy::type_complexity)]
    fn vector_leg(
        &self,
        qvec: &[f32],
    ) -> (Vec<ItemKey>, Vec<(i64, f32)>, HashMap<String, f32>) {
        let cache = self.embed_cache.read().unwrap();
        let mut scored: Vec<(ItemKey, f32)> = Vec::with_capacity(cache.items.len());
        let mut entity_sims = Vec::new();
        let mut fact_sims = HashMap::new();
        for (key, vector) in &cache.items {
            let sim = cosine(qvec, vector);
            match key {
                ItemKey::Entity(id) => entity_sims.push((*id, sim)),
                ItemKey::Fact(uid) => {
                    fact_sims.insert(uid.clone(), sim);
                    scored.push((key.clone(), sim));
                }
                _ => scored.push((key.clone(), sim)),
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        entity_sims.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        (
            scored.into_iter().take(LEG_TAKE).map(|(k, _)| k).collect(),
            entity_sims,
            fact_sims,
        )
    }

    async fn graph_leg(
        &self,
        query: &str,
        entity_sims: &[(i64, f32)],
        fact_sims: &HashMap<String, f32>,
    ) -> Result<Vec<ItemKey>> {
        // Seeds, strongest first: full name/alias substring in the query,
        // token-level name match ("Jane" seeds "Jane Rivera"), then top
        // cosine-similar entities.
        let mut seeds: HashMap<i64, f32> = HashMap::new();
        {
            let names = self.entity_names.read().unwrap();
            let q = crate::vault::norm_name(query);
            let q_words: std::collections::HashSet<&str> = q.split(' ').collect();
            for (norm, id) in names.iter() {
                if norm.is_empty() {
                    continue;
                }
                if q.contains(norm.as_str()) {
                    seeds.insert(*id, 1.0);
                } else if norm
                    .split(' ')
                    .any(|token| token.len() >= 4 && q_words.contains(token))
                {
                    seeds.entry(*id).or_insert(0.7);
                }
            }
        }
        for (id, sim) in entity_sims.iter().take(5) {
            if *sim >= 0.55 {
                seeds.entry(*id).or_insert(*sim);
            }
        }
        if seeds.is_empty() {
            return Ok(vec![]);
        }

        let edges =
            index::neighborhood(&self.store, seeds.keys().copied().collect(), 2, 512).await?;
        let ranks = personalized_pagerank(&edges, &seeds);

        // Facts inherit their subject's PPR rank; cosine similarity breaks
        // ties WITHIN an entity so "where does Alex work" prefers the work
        // fact among all Alex facts.
        let facts = self.fact_subjects.read().unwrap();
        let mut scored: Vec<(&String, f32, f32)> = facts
            .iter()
            .filter_map(|(uid, subject)| {
                subject.and_then(|s| ranks.get(&s)).map(|score| {
                    (uid, *score, fact_sims.get(uid).copied().unwrap_or(0.0))
                })
            })
            .filter(|(_, score, _)| *score > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        });
        Ok(scored
            .into_iter()
            .take(LEG_TAKE)
            .map(|(uid, ..)| ItemKey::Fact(uid.clone()))
            .collect())
    }

    /// Resolve keys into Memory items with text + provenance.
    async fn hydrate(&self, ranked: Vec<(ItemKey, f32, u8)>, limit: usize) -> Result<Vec<Memory>> {
        let fact_uids: Vec<String> = ranked
            .iter()
            .filter_map(|(key, ..)| match key {
                ItemKey::Fact(uid) => Some(uid.clone()),
                _ => None,
            })
            .collect();
        let fact_rows = if fact_uids.is_empty() {
            vec![]
        } else {
            index::facts_by_uids(&self.store, fact_uids).await?
        };
        let facts_by_uid: HashMap<&str, &index::FactRow> =
            fact_rows.iter().map(|f| (f.uid.as_str(), f)).collect();
        let note_titles = self.note_titles.read().unwrap().clone();

        let mut out = Vec::new();
        let mut per_note: HashMap<String, usize> = HashMap::new();
        for (key, score, legs) in ranked {
            if out.len() >= limit {
                break;
            }
            match key {
                ItemKey::Fact(uid) => {
                    let Some(fact) = facts_by_uid.get(uid.as_str()) else { continue };
                    if fact.expired_at.is_some() {
                        continue; // history is queryable, not injectable
                    }
                    // Diversity: at most 2 facts per note.
                    let seen = per_note.entry(fact.note_path.clone()).or_insert(0);
                    if *seen >= 2 {
                        continue;
                    }
                    *seen += 1;
                    out.push(Memory {
                        text: fact.text.clone(),
                        note: note_titles.get(&fact.note_path).cloned(),
                        note_path: Some(fact.note_path.clone()),
                        score,
                        legs,
                        provenance: Some(Provenance {
                            session_id: fact.source.map(|(s, _)| s),
                            message_id: fact.source.map(|(_, m)| m),
                            valid_from: fact.valid_from,
                            invalid_at: fact.invalid_at,
                        }),
                    });
                }
                ItemKey::Message(id) => {
                    let snippet = self.message_snippet(id).await.unwrap_or_default();
                    if snippet.is_empty() {
                        continue;
                    }
                    out.push(Memory {
                        text: snippet,
                        note: None,
                        note_path: None,
                        score,
                        legs,
                        provenance: Some(Provenance {
                            session_id: None,
                            message_id: Some(id),
                            valid_from: None,
                            invalid_at: None,
                        }),
                    });
                }
                ItemKey::Note(path) => {
                    let title = note_titles.get(&path).cloned().unwrap_or_else(|| path.clone());
                    out.push(Memory {
                        text: format!("see note '{title}'"),
                        note: Some(title),
                        note_path: Some(path),
                        score,
                        legs,
                        provenance: None,
                    });
                }
                ItemKey::Entity(_) => {}
            }
        }
        Ok(out)
    }

    async fn message_snippet(&self, message_id: i64) -> Result<String> {
        self.store
            .with(move |conn| {
                use rusqlite::OptionalExtension;
                let content: Option<String> = conn
                    .query_row("SELECT content FROM messages WHERE id = ?1", [message_id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                Ok(content.unwrap_or_default())
            })
            .await
            .map(|content| {
                // Extract text blocks from the stored JSON, truncated.
                let text = serde_json::from_str::<Vec<revenant_core::ContentBlock>>(&content)
                    .map(|blocks| {
                        blocks
                            .into_iter()
                            .filter_map(|block| match block {
                                revenant_core::ContentBlock::Text { text } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let mut snippet = text.trim().to_string();
                if snippet.len() > 240 {
                    let mut end = 240;
                    while end > 0 && !snippet.is_char_boundary(end) {
                        end -= 1;
                    }
                    snippet.truncate(end);
                    snippet.push('…');
                }
                snippet
            })
    }

    /// recall() rendered into a prompt block under a token budget. None when
    /// nothing scores above the floor — inject nothing rather than noise.
    pub async fn recall_block(&self, query: &str, budget_tokens: usize) -> Result<Option<String>> {
        let memories = self.recall(query, self.cfg.retrieval_limit).await?;
        if memories.is_empty() {
            return Ok(None);
        }
        let mut lines = Vec::new();
        let mut spent = 0usize;
        for memory in &memories {
            let label = match (&memory.note, &memory.provenance) {
                (Some(note), _) => note.clone(),
                (None, Some(p)) if p.message_id.is_some() => "conversation".to_string(),
                _ => "memory".to_string(),
            };
            let mut line = format!("- [{label}] {}", memory.text);
            if let Some(p) = &memory.provenance {
                if let Some(from) = p.valid_from {
                    line.push_str(&format!(" (since {})", fmt_date(from)));
                }
            }
            let cost = (line.len() as f64 / 3.6).ceil() as usize;
            if spent + cost > budget_tokens {
                break;
            }
            spent += cost;
            lines.push(line);
        }
        if lines.is_empty() {
            Ok(None)
        } else {
            Ok(Some(lines.join("\n")))
        }
    }
}

fn fmt_date(epoch: i64) -> String {
    // Days-to-civil (Howard Hinnant), good enough for YYYY-MM display.
    let days = epoch.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formatting() {
        assert_eq!(fmt_date(0), "1970-01");
        assert_eq!(fmt_date(1_704_067_200), "2024-01"); // 2024-01-01
    }
}
