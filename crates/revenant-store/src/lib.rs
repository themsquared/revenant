//! revenant-store: SQLite persistence behind a single-writer actor.
//!
//! All access goes through one dedicated thread owning the `Connection`,
//! so WAL-mode SQLite never sees concurrent writers and the rest of the
//! codebase stays async without `Mutex<Connection>` contention.

use anyhow::{Context, Result};
use revenant_core::{ContentBlock, Role, Usage};
use rusqlite::Connection;
use std::path::Path;
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Store {
    tx: mpsc::UnboundedSender<Job>,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: i64,
    pub turn: i64,
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        std::thread::Builder::new()
            .name("revenant-store".into())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    job(&mut conn);
                }
            })
            .context("spawning store thread")?;
        Ok(Store { tx })
    }

    /// Run a closure on the store thread and await its result.
    pub async fn with<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |conn| {
                let _ = tx.send(f(conn));
            }))
            .map_err(|_| anyhow::anyhow!("store thread is gone"))?;
        Ok(rx.await.context("store thread dropped reply")??)
    }

    /// Find or create a session for (channel, peer, kind).
    pub async fn ensure_session(&self, channel: &str, peer: &str, kind: &str) -> Result<i64> {
        let (channel, peer, kind) = (channel.to_owned(), peer.to_owned(), kind.to_owned());
        self.with(move |conn| {
            let now = unix_now();
            conn.execute(
                "INSERT INTO sessions (channel, peer, kind, created_at, last_active)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(channel, peer, kind) DO UPDATE SET last_active = ?4",
                (&channel, &peer, &kind, now),
            )?;
            conn.query_row(
                "SELECT id FROM sessions WHERE channel = ?1 AND peer = ?2 AND kind = ?3",
                (&channel, &peer, &kind),
                |row| row.get(0),
            )
        })
        .await
    }

    pub async fn append_message(
        &self,
        session_id: i64,
        role: Role,
        content: &[ContentBlock],
        token_estimate: Option<i64>,
    ) -> Result<i64> {
        let content_json = serde_json::to_string(content)?;
        self.with(move |conn| {
            let now = unix_now();
            conn.execute(
                "INSERT INTO messages (session_id, turn, role, content, token_estimate, created_at)
                 VALUES (?1,
                         COALESCE((SELECT MAX(turn) FROM messages WHERE session_id = ?1), 0) + 1,
                         ?2, ?3, ?4, ?5)",
                (session_id, role.as_str(), &content_json, token_estimate, now),
            )?;
            conn.execute(
                "UPDATE sessions SET last_active = ?2 WHERE id = ?1",
                (session_id, now),
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// The most recent `limit` non-compacted messages, oldest first.
    pub async fn history(&self, session_id: i64, limit: usize) -> Result<Vec<StoredMessage>> {
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, turn, role, content FROM (
                   SELECT id, turn, role, content FROM messages
                   WHERE session_id = ?1 AND compacted_into IS NULL
                   ORDER BY turn DESC LIMIT ?2
                 ) ORDER BY turn ASC",
            )?;
            let rows = stmt.query_map((session_id, limit as i64), |row| {
                let role: String = row.get(2)?;
                let content: String = row.get(3)?;
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, role, content))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, turn, role, content) = row?;
                let role = match role.as_str() {
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                };
                let content: Vec<ContentBlock> =
                    serde_json::from_str(&content).unwrap_or_else(|_| vec![]);
                out.push(StoredMessage { id, turn, role, content });
            }
            Ok(out)
        })
        .await
    }

    pub async fn record_spend(
        &self,
        session_id: i64,
        tier: &str,
        model: Option<&str>,
        usage: Usage,
    ) -> Result<()> {
        let tier = tier.to_owned();
        let model = model.map(|s| s.to_owned());
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO spend_ledger
                   (at, session_id, tier, model, tokens_in, tokens_out, cache_read, cache_write, source)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'response_usage')",
                (
                    unix_now(),
                    session_id,
                    &tier,
                    &model,
                    usage.input_tokens as i64,
                    usage.output_tokens as i64,
                    usage.cache_read_input_tokens as i64,
                    usage.cache_creation_input_tokens as i64,
                ),
            )?;
            Ok(())
        })
        .await
    }

    /// Total tokens spent today (UTC), for the chat footer.
    pub async fn spend_today(&self) -> Result<(i64, i64)> {
        self.with(|conn| {
            let day_start = unix_now() - (unix_now() % 86_400);
            conn.query_row(
                "SELECT COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0)
                 FROM spend_ledger WHERE at >= ?1",
                [day_start],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE sessions (
               id INTEGER PRIMARY KEY,
               channel TEXT NOT NULL,
               peer TEXT NOT NULL,
               kind TEXT NOT NULL DEFAULT 'chat',
               parent_session INTEGER REFERENCES sessions(id),
               created_at INTEGER NOT NULL,
               last_active INTEGER NOT NULL,
               archived INTEGER NOT NULL DEFAULT 0,
               UNIQUE(channel, peer, kind)
             );
             CREATE TABLE messages (
               id INTEGER PRIMARY KEY,
               session_id INTEGER NOT NULL REFERENCES sessions(id),
               turn INTEGER NOT NULL,
               role TEXT NOT NULL,
               content TEXT NOT NULL,
               token_estimate INTEGER,
               created_at INTEGER NOT NULL,
               compacted_into INTEGER
             );
             CREATE INDEX idx_messages_session ON messages(session_id, turn);
             CREATE TABLE spend_ledger (
               id INTEGER PRIMARY KEY,
               at INTEGER NOT NULL,
               session_id INTEGER,
               loop_id TEXT,
               tier TEXT,
               model TEXT,
               tokens_in INTEGER NOT NULL DEFAULT 0,
               tokens_out INTEGER NOT NULL DEFAULT 0,
               cache_read INTEGER NOT NULL DEFAULT 0,
               cache_write INTEGER NOT NULL DEFAULT 0,
               cost_usd REAL,
               source TEXT NOT NULL
             );
             CREATE INDEX idx_spend_at ON spend_ledger(at);
             CREATE TABLE approvals (
               id TEXT PRIMARY KEY,
               kind TEXT NOT NULL,
               payload TEXT NOT NULL,
               requested_at INTEGER NOT NULL,
               resolved_at INTEGER,
               verdict TEXT,
               resolver TEXT,
               ttl_s INTEGER NOT NULL DEFAULT 900
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let dir = std::env::temp_dir().join(format!("revenant-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let sid = store.ensure_session("cli", "local", "chat").await.unwrap();
        let sid2 = store.ensure_session("cli", "local", "chat").await.unwrap();
        assert_eq!(sid, sid2);

        store
            .append_message(sid, Role::User, &[ContentBlock::text("hello")], Some(2))
            .await
            .unwrap();
        store
            .append_message(sid, Role::Assistant, &[ContentBlock::text("hi!")], Some(2))
            .await
            .unwrap();
        let hist = store.history(sid, 10).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[1].turn, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
