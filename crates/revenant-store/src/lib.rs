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

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecallHit {
    pub snippet: String,
    pub source: String,
    pub reference: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApprovalRow {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub requested_at: i64,
    pub ttl_s: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRow {
    pub id: i64,
    pub channel: String,
    pub peer: String,
    pub kind: String,
    pub last_active: i64,
    pub message_count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SpendRow {
    pub model: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub requests: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SubagentRow {
    pub id: i64,
    pub parent_session: Option<i64>,
    pub created_at: i64,
    pub last_active: i64,
    pub message_count: i64,
    /// JSON of the first user message content (the task), if any.
    pub first_user: Option<String>,
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

    /// Create a fresh child (subagent) session under a parent.
    pub async fn create_child_session(&self, parent: i64, label: &str) -> Result<i64> {
        let label = label.to_owned();
        self.with(move |conn| {
            let now = unix_now();
            // Unique peer per child so each spawn is its own session.
            let peer = format!("{parent}-{now}-{}", conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))?);
            conn.execute(
                "INSERT INTO sessions (channel, peer, kind, parent_session, created_at, last_active)
                 VALUES ('subagent', ?1, 'subagent', ?2, ?3, ?3)",
                (&peer, parent, now),
            )?;
            let id = conn.last_insert_rowid();
            // Stash the human task label as the session's first marker via peer;
            // label is also surfaced through the spawned event.
            let _ = label;
            Ok(id)
        })
        .await
    }

    /// Subagent sessions, newest first, with parent + message count.
    pub async fn subagents_list(&self, limit: usize) -> Result<Vec<SubagentRow>> {
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.parent_session, s.created_at, s.last_active,
                        (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id),
                        (SELECT content FROM messages m WHERE m.session_id = s.id AND m.role = 'user'
                         ORDER BY m.turn ASC LIMIT 1)
                 FROM sessions s WHERE s.kind = 'subagent'
                 ORDER BY s.id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit as i64], |r| {
                Ok(SubagentRow {
                    id: r.get(0)?,
                    parent_session: r.get(1)?,
                    created_at: r.get(2)?,
                    last_active: r.get(3)?,
                    message_count: r.get(4)?,
                    first_user: r.get(5)?,
                })
            })?;
            rows.collect()
        })
        .await
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
        // Index human-meaningful text for recall (skip tool plumbing).
        let recall_text: String = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.with(move |conn| {
            let now = unix_now();
            conn.execute(
                "INSERT INTO messages (session_id, turn, role, content, token_estimate, created_at)
                 VALUES (?1,
                         COALESCE((SELECT MAX(turn) FROM messages WHERE session_id = ?1), 0) + 1,
                         ?2, ?3, ?4, ?5)",
                (session_id, role.as_str(), &content_json, token_estimate, now),
            )?;
            let message_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE sessions SET last_active = ?2 WHERE id = ?1",
                (session_id, now),
            )?;
            if !recall_text.is_empty() {
                conn.execute(
                    "INSERT INTO recall (text, source, ref) VALUES (?1, 'message', ?2)",
                    (&recall_text, message_id.to_string()),
                )?;
            }
            Ok(message_id)
        })
        .await
    }

    /// FTS5 search across indexed conversation text and memory notes.
    pub async fn recall_search(&self, query: &str, limit: usize) -> Result<Vec<RecallHit>> {
        // FTS5 MATCH has its own query syntax; quote each term to keep user
        // input from being interpreted as operators. OR-join: natural-language
        // queries rarely contain every document term, and BM25 ranking plus
        // downstream RRF fusion handle the extra recall.
        let sanitized: String = query
            .split_whitespace()
            .map(|term| format!("\"{}\"", term.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT snippet(recall, 0, '[', ']', '…', 24), source, ref
                 FROM recall WHERE recall MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map((sanitized, limit as i64), |row| {
                Ok(RecallHit {
                    snippet: row.get(0)?,
                    source: row.get(1)?,
                    reference: row.get(2)?,
                })
            })?;
            rows.collect()
        })
        .await
    }

    /// Index an external document (memory file, note) for recall. Replaces
    /// prior rows with the same ref.
    pub async fn recall_index(&self, source: &str, reference: &str, text: &str) -> Result<()> {
        let (source, reference, text) =
            (source.to_owned(), reference.to_owned(), text.to_owned());
        self.with(move |conn| {
            conn.execute("DELETE FROM recall WHERE ref = ?1 AND source = ?2", (&reference, &source))?;
            conn.execute(
                "INSERT INTO recall (text, source, ref) VALUES (?1, ?2, ?3)",
                (&text, &source, &reference),
            )?;
            Ok(())
        })
        .await
    }

    // ---- approvals ----

    pub async fn approval_insert(
        &self,
        id: &str,
        kind: &str,
        payload: &str,
        ttl_s: i64,
    ) -> Result<()> {
        let (id, kind, payload) = (id.to_owned(), kind.to_owned(), payload.to_owned());
        self.with(move |conn| {
            conn.execute(
                "INSERT INTO approvals (id, kind, payload, requested_at, ttl_s)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (&id, &kind, &payload, unix_now(), ttl_s),
            )?;
            Ok(())
        })
        .await
    }

    /// Compare-and-swap resolve: returns false if already resolved.
    pub async fn approval_resolve(&self, id: &str, verdict: &str, resolver: &str) -> Result<bool> {
        let (id, verdict, resolver) = (id.to_owned(), verdict.to_owned(), resolver.to_owned());
        self.with(move |conn| {
            let n = conn.execute(
                "UPDATE approvals SET resolved_at = ?2, verdict = ?3, resolver = ?4
                 WHERE id = ?1 AND resolved_at IS NULL",
                (&id, unix_now(), &verdict, &resolver),
            )?;
            Ok(n == 1)
        })
        .await
    }

    pub async fn approvals_pending(&self) -> Result<Vec<ApprovalRow>> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, kind, payload, requested_at, ttl_s FROM approvals
                 WHERE resolved_at IS NULL AND requested_at + ttl_s > ?1
                 ORDER BY requested_at ASC",
            )?;
            let rows = stmt.query_map([unix_now()], |row| {
                Ok(ApprovalRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    payload: row.get(2)?,
                    requested_at: row.get(3)?,
                    ttl_s: row.get(4)?,
                })
            })?;
            rows.collect()
        })
        .await
    }

    // ---- sessions / spend for the control plane ----

    pub async fn sessions_list(&self, limit: usize) -> Result<Vec<SessionRow>> {
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.channel, s.peer, s.kind, s.last_active,
                        (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
                 FROM sessions s WHERE s.archived = 0
                 ORDER BY s.last_active DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit as i64], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    peer: row.get(2)?,
                    kind: row.get(3)?,
                    last_active: row.get(4)?,
                    message_count: row.get(5)?,
                })
            })?;
            rows.collect()
        })
        .await
    }

    // ---- channel pairing ----

    pub async fn pairing_code_create(&self, code: &str, ttl_s: i64) -> Result<()> {
        let code = code.to_owned();
        self.with(move |conn| {
            let now = unix_now();
            conn.execute(
                "INSERT INTO pairing_codes (code, created_at, expires_at) VALUES (?1, ?2, ?3)",
                (&code, now, now + ttl_s),
            )?;
            Ok(())
        })
        .await
    }

    /// Claim a pairing code (single-use, unexpired). On success the peer is
    /// added to the channel allowlist.
    pub async fn pairing_claim(&self, code: &str, channel: &str, peer: &str) -> Result<bool> {
        let (code, channel, peer) = (code.to_owned(), channel.to_owned(), peer.to_owned());
        self.with(move |conn| {
            let now = unix_now();
            let claimed = conn.execute(
                "UPDATE pairing_codes SET used_by = ?2
                 WHERE code = ?1 AND used_by IS NULL AND expires_at > ?3",
                (&code, format!("{channel}:{peer}"), now),
            )?;
            if claimed == 1 {
                conn.execute(
                    "INSERT OR REPLACE INTO channel_pairings (channel, peer, created_at)
                     VALUES (?1, ?2, ?3)",
                    (&channel, &peer, now),
                )?;
            }
            Ok(claimed == 1)
        })
        .await
    }

    pub async fn peer_allowed(&self, channel: &str, peer: &str) -> Result<bool> {
        let (channel, peer) = (channel.to_owned(), peer.to_owned());
        self.with(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM channel_pairings WHERE channel = ?1 AND peer = ?2",
                (&channel, &peer),
                |r| r.get(0),
            )?;
            Ok(count > 0)
        })
        .await
    }

    pub async fn peers_list(&self, channel: &str) -> Result<Vec<String>> {
        let channel = channel.to_owned();
        self.with(move |conn| {
            let mut stmt =
                conn.prepare("SELECT peer FROM channel_pairings WHERE channel = ?1")?;
            let rows = stmt.query_map([&channel], |r| r.get(0))?;
            rows.collect()
        })
        .await
    }

    /// Spend grouped by model since `from_ts`.
    pub async fn spend_since(&self, from_ts: i64) -> Result<Vec<SpendRow>> {
        self.with(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT COALESCE(model, tier, 'unknown'),
                        SUM(tokens_in), SUM(tokens_out), COUNT(*)
                 FROM spend_ledger WHERE at >= ?1
                 GROUP BY 1 ORDER BY SUM(tokens_in) + SUM(tokens_out) DESC",
            )?;
            let rows = stmt.query_map([from_ts], |row| {
                Ok(SpendRow {
                    model: row.get(0)?,
                    tokens_in: row.get(1)?,
                    tokens_out: row.get(2)?,
                    requests: row.get(3)?,
                })
            })?;
            rows.collect()
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
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 2 {
        conn.execute_batch(
            "BEGIN;
             CREATE VIRTUAL TABLE recall USING fts5(
               text, source, ref, tokenize='porter unicode61'
             );
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 3 {
        // Memory engine: markdown vault is source of truth; these tables are
        // the rebuildable derived index. Facts/edges are BI-TEMPORAL —
        // expired_at is set on supersession, rows are never deleted.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE mem_entities (
               id          INTEGER PRIMARY KEY,
               uid         TEXT NOT NULL UNIQUE,
               name        TEXT NOT NULL,
               norm_name   TEXT NOT NULL UNIQUE,
               kind        TEXT NOT NULL DEFAULT 'concept',
               note_path   TEXT NOT NULL UNIQUE,
               summary     TEXT,
               embedding   BLOB,
               created_at  INTEGER NOT NULL,
               updated_at  INTEGER NOT NULL
             );
             CREATE TABLE mem_aliases (
               alias_norm  TEXT PRIMARY KEY,
               entity_id   INTEGER NOT NULL REFERENCES mem_entities(id)
             );
             CREATE TABLE mem_facts (
               id          INTEGER PRIMARY KEY,
               uid         TEXT NOT NULL UNIQUE,
               note_path   TEXT NOT NULL,
               subject_id  INTEGER REFERENCES mem_entities(id),
               predicate   TEXT,
               object      TEXT,
               text        TEXT NOT NULL,
               embedding   BLOB,
               valid_from  INTEGER,
               invalid_at  INTEGER,
               recorded_at INTEGER NOT NULL,
               expired_at  INTEGER,
               source_session_id INTEGER,
               source_message_id INTEGER
             );
             CREATE INDEX idx_mem_facts_note ON mem_facts(note_path) WHERE expired_at IS NULL;
             CREATE INDEX idx_mem_facts_subj ON mem_facts(subject_id) WHERE expired_at IS NULL;
             CREATE TABLE mem_edges (
               id          INTEGER PRIMARY KEY,
               src         INTEGER NOT NULL REFERENCES mem_entities(id),
               dst         INTEGER NOT NULL REFERENCES mem_entities(id),
               rel         TEXT NOT NULL,
               weight      REAL NOT NULL DEFAULT 1.0,
               fact_id     INTEGER REFERENCES mem_facts(id),
               valid_from  INTEGER,
               invalid_at  INTEGER,
               recorded_at INTEGER NOT NULL,
               expired_at  INTEGER
             );
             CREATE INDEX idx_mem_edges_src ON mem_edges(src) WHERE expired_at IS NULL;
             CREATE INDEX idx_mem_edges_dst ON mem_edges(dst) WHERE expired_at IS NULL;
             CREATE TABLE mem_meta (
               k TEXT PRIMARY KEY, v TEXT NOT NULL
             );
             CREATE TABLE mem_pending (
               id         INTEGER PRIMARY KEY,
               kind       TEXT NOT NULL,
               payload    TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               attempts   INTEGER NOT NULL DEFAULT 0
             );
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 4 {
        // Channel pairing: which chat peers may talk to this agent, and
        // one-time codes that grant pairing.
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE channel_pairings (
               channel    TEXT NOT NULL,
               peer       TEXT NOT NULL,
               label      TEXT,
               created_at INTEGER NOT NULL,
               PRIMARY KEY (channel, peer)
             );
             CREATE TABLE pairing_codes (
               code       TEXT PRIMARY KEY,
               created_at INTEGER NOT NULL,
               expires_at INTEGER NOT NULL,
               used_by    TEXT
             );
             PRAGMA user_version = 4;
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

        // FTS recall over message text
        let hits = store.recall_search("hello", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "message");

        // approvals CAS
        store.approval_insert("ap1", "exec", "{}", 900).await.unwrap();
        assert_eq!(store.approvals_pending().await.unwrap().len(), 1);
        assert!(store.approval_resolve("ap1", "approved", "test").await.unwrap());
        assert!(!store.approval_resolve("ap1", "denied", "late").await.unwrap());
        assert_eq!(store.approvals_pending().await.unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pairing_flow() {
        let dir = std::env::temp_dir().join(format!("revenant-pair-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();

        store.pairing_code_create("ABCD2345", 600).await.unwrap();
        assert!(!store.peer_allowed("telegram", "111").await.unwrap());
        // Claim works once, adds the peer.
        assert!(store.pairing_claim("ABCD2345", "telegram", "111").await.unwrap());
        assert!(store.peer_allowed("telegram", "111").await.unwrap());
        // Single use.
        assert!(!store.pairing_claim("ABCD2345", "telegram", "222").await.unwrap());
        // Unknown code.
        assert!(!store.pairing_claim("NOPE9999", "telegram", "333").await.unwrap());
        // Expired code.
        store.pairing_code_create("EXPIRED1", -10).await.unwrap();
        assert!(!store.pairing_claim("EXPIRED1", "telegram", "444").await.unwrap());
        assert_eq!(store.peers_list("telegram").await.unwrap(), vec!["111"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
