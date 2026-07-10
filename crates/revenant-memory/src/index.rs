//! SQL for the mem_* derived index. All access via the store's single-writer
//! actor; the vault is the source of truth and these tables are rebuildable.

use anyhow::Result;
use revenant_store::Store;

#[derive(Debug, Clone)]
pub struct NewEntity {
    pub uid: String,
    pub name: String,
    pub norm_name: String,
    pub kind: String,
    pub note_path: String,
    pub summary: Option<String>,
    pub embedding: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub uid: String,
    pub note_path: String,
    pub subject_id: Option<i64>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub text: String,
    pub embedding: Option<Vec<u8>>,
    pub valid_from: Option<i64>,
    pub invalid_at: Option<i64>,
    pub expired_at: Option<i64>,
    pub source_session_id: Option<i64>,
    pub source_message_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct EntityRow {
    pub id: i64,
    pub uid: String,
    pub name: String,
    pub norm_name: String,
    pub kind: String,
    pub note_path: String,
    pub embedding: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct FactRow {
    pub id: i64,
    pub uid: String,
    pub note_path: String,
    pub subject_id: Option<i64>,
    pub text: String,
    pub valid_from: Option<i64>,
    pub invalid_at: Option<i64>,
    pub expired_at: Option<i64>,
    pub embedding: Option<Vec<u8>>,
    pub source: Option<(i64, i64)>,
}

#[derive(Debug, Clone, Copy)]
pub struct EdgeRow {
    pub src: i64,
    pub dst: i64,
    pub weight: f32,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub async fn upsert_entity(store: &Store, e: NewEntity) -> Result<i64> {
    store
        .with(move |conn| {
            let ts = now();
            conn.execute(
                "INSERT INTO mem_entities (uid, name, norm_name, kind, note_path, summary, embedding, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(uid) DO UPDATE SET
                   name = excluded.name, norm_name = excluded.norm_name,
                   kind = excluded.kind, note_path = excluded.note_path,
                   summary = excluded.summary, embedding = excluded.embedding,
                   updated_at = excluded.updated_at",
                rusqlite::params![e.uid, e.name, e.norm_name, e.kind, e.note_path, e.summary, e.embedding, ts],
            )?;
            conn.query_row("SELECT id FROM mem_entities WHERE uid = ?1", [&e.uid], |r| r.get(0))
        })
        .await
}

pub async fn add_alias(store: &Store, alias_norm: &str, entity_id: i64) -> Result<()> {
    let alias = alias_norm.to_owned();
    store
        .with(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO mem_aliases (alias_norm, entity_id) VALUES (?1, ?2)",
                rusqlite::params![alias, entity_id],
            )?;
            Ok(())
        })
        .await
}

pub async fn insert_fact(store: &Store, f: NewFact) -> Result<i64> {
    store
        .with(move |conn| {
            conn.execute(
                "INSERT INTO mem_facts
                   (uid, note_path, subject_id, predicate, object, text, embedding,
                    valid_from, invalid_at, recorded_at, expired_at,
                    source_session_id, source_message_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                 ON CONFLICT(uid) DO UPDATE SET
                   note_path=excluded.note_path, subject_id=excluded.subject_id,
                   predicate=excluded.predicate, object=excluded.object,
                   text=excluded.text, embedding=excluded.embedding,
                   valid_from=excluded.valid_from, invalid_at=excluded.invalid_at,
                   expired_at=excluded.expired_at",
                rusqlite::params![
                    f.uid, f.note_path, f.subject_id, f.predicate, f.object, f.text,
                    f.embedding, f.valid_from, f.invalid_at, now(), f.expired_at,
                    f.source_session_id, f.source_message_id
                ],
            )?;
            conn.query_row("SELECT id FROM mem_facts WHERE uid = ?1", [&f.uid], |r| r.get(0))
        })
        .await
}

pub async fn insert_edge(
    store: &Store,
    src: i64,
    dst: i64,
    rel: &str,
    weight: f32,
    fact_id: Option<i64>,
) -> Result<()> {
    let rel = rel.to_owned();
    store
        .with(move |conn| {
            conn.execute(
                "INSERT INTO mem_edges (src, dst, rel, weight, fact_id, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![src, dst, rel, weight, fact_id, now()],
            )?;
            Ok(())
        })
        .await
}

/// Bi-temporal supersession: expire (never delete) a fact and its edges.
pub async fn expire_fact(store: &Store, fact_uid: &str, invalid_at: Option<i64>) -> Result<bool> {
    let uid = fact_uid.to_owned();
    store
        .with(move |conn| {
            let ts = now();
            let n = conn.execute(
                "UPDATE mem_facts SET expired_at = ?2, invalid_at = COALESCE(?3, invalid_at, ?2)
                 WHERE uid = ?1 AND expired_at IS NULL",
                rusqlite::params![uid, ts, invalid_at],
            )?;
            conn.execute(
                "UPDATE mem_edges SET expired_at = ?2
                 WHERE fact_id = (SELECT id FROM mem_facts WHERE uid = ?1) AND expired_at IS NULL",
                rusqlite::params![uid, ts],
            )?;
            Ok(n > 0)
        })
        .await
}

pub async fn entities_all(store: &Store) -> Result<Vec<EntityRow>> {
    store
        .with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, uid, name, norm_name, kind, note_path, embedding FROM mem_entities",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(EntityRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    name: r.get(2)?,
                    norm_name: r.get(3)?,
                    kind: r.get(4)?,
                    note_path: r.get(5)?,
                    embedding: r.get(6)?,
                })
            })?;
            rows.collect()
        })
        .await
}

pub async fn facts_active(store: &Store) -> Result<Vec<FactRow>> {
    store
        .with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, uid, note_path, subject_id, text, valid_from, invalid_at, expired_at,
                        embedding, source_session_id, source_message_id
                 FROM mem_facts WHERE expired_at IS NULL",
            )?;
            let rows = stmt.query_map([], map_fact)?;
            rows.collect()
        })
        .await
}

pub async fn facts_by_uids(store: &Store, uids: Vec<String>) -> Result<Vec<FactRow>> {
    store
        .with(move |conn| {
            let placeholders = uids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT id, uid, note_path, subject_id, text, valid_from, invalid_at, expired_at,
                        embedding, source_session_id, source_message_id
                 FROM mem_facts WHERE uid IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(uids.iter()), map_fact)?;
            rows.collect()
        })
        .await
}

fn map_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<FactRow> {
    let session: Option<i64> = r.get(9)?;
    let message: Option<i64> = r.get(10)?;
    Ok(FactRow {
        id: r.get(0)?,
        uid: r.get(1)?,
        note_path: r.get(2)?,
        subject_id: r.get(3)?,
        text: r.get(4)?,
        valid_from: r.get(5)?,
        invalid_at: r.get(6)?,
        expired_at: r.get(7)?,
        embedding: r.get(8)?,
        source: session.zip(message),
    })
}

/// Active edges within the ≤`depth`-hop neighborhood of `seeds`.
pub async fn neighborhood(
    store: &Store,
    seeds: Vec<i64>,
    depth: u8,
    cap: usize,
) -> Result<Vec<EdgeRow>> {
    if seeds.is_empty() {
        return Ok(vec![]);
    }
    store
        .with(move |conn| {
            let seed_list = seeds
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "WITH RECURSIVE hood(id, depth) AS (
                   SELECT id, 0 FROM mem_entities WHERE id IN ({seed_list})
                   UNION
                   SELECT CASE WHEN e.src = h.id THEN e.dst ELSE e.src END, h.depth + 1
                   FROM mem_edges e JOIN hood h ON (e.src = h.id OR e.dst = h.id)
                   WHERE h.depth < {depth} AND e.expired_at IS NULL
                 )
                 SELECT e.src, e.dst, e.weight FROM mem_edges e
                 WHERE e.expired_at IS NULL
                   AND e.src IN (SELECT id FROM hood)
                   AND e.dst IN (SELECT id FROM hood)
                 LIMIT {cap}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(EdgeRow { src: r.get(0)?, dst: r.get(1)?, weight: r.get::<_, f64>(2)? as f32 })
            })?;
            rows.collect()
        })
        .await
}

pub async fn meta_get(store: &Store, key: &str) -> Result<Option<String>> {
    let key = key.to_owned();
    store
        .with(move |conn| {
            use rusqlite::OptionalExtension;
            conn.query_row("SELECT v FROM mem_meta WHERE k = ?1", [&key], |r| r.get(0))
                .optional()
        })
        .await
}

pub async fn meta_set(store: &Store, key: &str, value: &str) -> Result<()> {
    let (key, value) = (key.to_owned(), value.to_owned());
    store
        .with(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO mem_meta (k, v) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )?;
            Ok(())
        })
        .await
}

#[derive(Debug, Clone)]
pub struct PendingRow {
    pub id: i64,
    pub kind: String,
    pub payload: String,
    pub attempts: i64,
}

pub async fn pending_list(store: &Store, kind: &str, limit: usize) -> Result<Vec<PendingRow>> {
    let kind = kind.to_owned();
    store
        .with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, kind, payload, attempts FROM mem_pending
                 WHERE kind = ?1 AND attempts < 5 ORDER BY id ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![kind, limit as i64], |r| {
                Ok(PendingRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    payload: r.get(2)?,
                    attempts: r.get(3)?,
                })
            })?;
            rows.collect()
        })
        .await
}

pub async fn pending_delete(store: &Store, ids: Vec<i64>) -> Result<()> {
    store
        .with(move |conn| {
            for id in ids {
                conn.execute("DELETE FROM mem_pending WHERE id = ?1", [id])?;
            }
            Ok(())
        })
        .await
}

pub async fn pending_bump_attempts(store: &Store, ids: Vec<i64>) -> Result<()> {
    store
        .with(move |conn| {
            for id in ids {
                conn.execute(
                    "UPDATE mem_pending SET attempts = attempts + 1 WHERE id = ?1",
                    [id],
                )?;
            }
            Ok(())
        })
        .await
}

pub async fn pending_push(store: &Store, kind: &str, payload: &str) -> Result<()> {
    let (kind, payload) = (kind.to_owned(), payload.to_owned());
    store
        .with(move |conn| {
            conn.execute(
                "INSERT INTO mem_pending (kind, payload, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![kind, payload, now()],
            )?;
            Ok(())
        })
        .await
}

pub async fn counts(store: &Store) -> Result<(i64, i64, i64, i64)> {
    store
        .with(|conn| {
            let entities: i64 =
                conn.query_row("SELECT COUNT(*) FROM mem_entities", [], |r| r.get(0))?;
            let facts: i64 = conn.query_row(
                "SELECT COUNT(*) FROM mem_facts WHERE expired_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let edges: i64 = conn.query_row(
                "SELECT COUNT(*) FROM mem_edges WHERE expired_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let pending: i64 =
                conn.query_row("SELECT COUNT(*) FROM mem_pending", [], |r| r.get(0))?;
            Ok((entities, facts, edges, pending))
        })
        .await
}

/// Reindex: wipe everything derived (mem_* + vault/fact rows in recall FTS).
pub async fn wipe_derived(store: &Store) -> Result<()> {
    store
        .with(|conn| {
            conn.execute_batch(
                "DELETE FROM mem_edges;
                 DELETE FROM mem_facts;
                 DELETE FROM mem_aliases;
                 DELETE FROM mem_entities;
                 DELETE FROM recall WHERE source IN ('fact', 'vault');",
            )?;
            Ok(())
        })
        .await
}
