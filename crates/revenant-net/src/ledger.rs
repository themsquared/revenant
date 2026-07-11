//! The ledger: a durable, append-only, hash-linked transparency log. Every
//! event (an artifact published, an adoption attested) becomes an entry chained
//! to the one before it — `hash = sha256(prev_hash || kind || body)` — so the
//! whole history is tamper-evident: you cannot alter or reorder the past
//! without breaking every hash that follows. The Necropolis catalog is *derived*
//! by replaying this log, and a replica syncs by pulling entries since its head
//! and re-verifying the chain. This is the Rekor/Certificate-Transparency model
//! — federation without mining, tokens, or global consensus.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

pub const GENESIS: &str = "genesis";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    /// Raw JSON body, stored and transferred verbatim so the hash a replica
    /// recomputes matches the origin's byte-for-byte.
    pub body: String,
    pub prev_hash: String,
    pub hash: String,
}

pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Open (or create) a ledger. Pass `":memory:"` for an ephemeral one.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening ledger at {path}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ledger (
                seq       INTEGER PRIMARY KEY AUTOINCREMENT,
                ts        INTEGER NOT NULL,
                kind      TEXT NOT NULL,
                body      TEXT NOT NULL,
                prev_hash TEXT NOT NULL,
                hash      TEXT NOT NULL
            );",
        )?;
        Ok(Ledger { conn })
    }

    /// The hash chained into the next entry — `GENESIS` when empty.
    pub fn head_hash(&self) -> Result<String> {
        let h: Option<String> = self
            .conn
            .query_row("SELECT hash FROM ledger ORDER BY seq DESC LIMIT 1", [], |r| r.get(0))
            .ok();
        Ok(h.unwrap_or_else(|| GENESIS.to_string()))
    }

    pub fn head_seq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM ledger", [], |r| r.get(0))
            .unwrap_or(0))
    }

    /// Compute the entry hash — the single definition both append and verify
    /// (and every replica) must agree on.
    pub fn entry_hash(prev_hash: &str, kind: &str, body: &str) -> String {
        let mut h = Sha256::new();
        h.update(prev_hash.as_bytes());
        h.update([0]);
        h.update(kind.as_bytes());
        h.update([0]);
        h.update(body.as_bytes());
        hex::encode(h.finalize())
    }

    /// Append a new event, chaining it to the current head.
    pub fn append(&self, kind: &str, body: &str, ts: i64) -> Result<Entry> {
        let prev_hash = self.head_hash()?;
        let hash = Self::entry_hash(&prev_hash, kind, body);
        self.conn.execute(
            "INSERT INTO ledger (ts, kind, body, prev_hash, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![ts, kind, body, prev_hash, hash],
        )?;
        let seq = self.conn.last_insert_rowid();
        Ok(Entry { seq, ts, kind: kind.into(), body: body.into(), prev_hash, hash })
    }

    /// Append an entry received from another node, but ONLY if it chains
    /// correctly onto our head and its hash checks out. Fails closed — a
    /// replica can never be fed a broken or forked chain.
    pub fn append_verified(&self, e: &Entry) -> Result<()> {
        let head = self.head_hash()?;
        if e.prev_hash != head {
            bail!("entry {} prev_hash does not chain onto our head", e.seq);
        }
        let expect = Self::entry_hash(&e.prev_hash, &e.kind, &e.body);
        if expect != e.hash {
            bail!("entry {} hash mismatch — refusing", e.seq);
        }
        self.conn.execute(
            "INSERT INTO ledger (ts, kind, body, prev_hash, hash) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![e.ts, e.kind, e.body, e.prev_hash, e.hash],
        )?;
        Ok(())
    }

    /// Entries with seq > `since`, in order — for replica sync and replay.
    pub fn since(&self, since: i64) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, kind, body, prev_hash, hash FROM ledger WHERE seq > ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(Entry {
                seq: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                body: r.get(3)?,
                prev_hash: r.get(4)?,
                hash: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Recompute the whole chain and confirm every link — the audit that makes
    /// tampering detectable. Returns the number of entries verified.
    pub fn verify_chain(&self) -> Result<usize> {
        let mut prev = GENESIS.to_string();
        let mut n = 0;
        for e in self.since(0)? {
            let expect = Self::entry_hash(&prev, &e.kind, &e.body);
            if e.prev_hash != prev {
                bail!("chain break at seq {}: prev_hash mismatch", e.seq);
            }
            if e.hash != expect {
                bail!("chain break at seq {}: body was tampered", e.seq);
            }
            prev = e.hash;
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chains_and_verifies() {
        let l = Ledger::open(":memory:").unwrap();
        l.append("artifact", r#"{"id":"a"}"#, 1).unwrap();
        l.append("attest", r#"{"id":"a","passed":true}"#, 2).unwrap();
        let e3 = l.append("artifact", r#"{"id":"b"}"#, 3).unwrap();
        assert_eq!(l.verify_chain().unwrap(), 3);
        assert_eq!(l.head_hash().unwrap(), e3.hash);
        assert_eq!(l.head_seq().unwrap(), 3);
    }

    #[test]
    fn tampering_a_past_entry_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("l.db").to_string_lossy().to_string();
        {
            let l = Ledger::open(&p).unwrap();
            l.append("artifact", r#"{"id":"a"}"#, 1).unwrap();
            l.append("artifact", r#"{"id":"b"}"#, 2).unwrap();
            assert_eq!(l.verify_chain().unwrap(), 2);
        }
        // Rewrite history behind the ledger's back.
        {
            let c = Connection::open(&p).unwrap();
            c.execute("UPDATE ledger SET body = ?1 WHERE seq = 1", [r#"{"id":"evil"}"#]).unwrap();
        }
        let l = Ledger::open(&p).unwrap();
        assert!(l.verify_chain().is_err(), "tampered chain must fail verification");
    }

    #[test]
    fn replica_only_accepts_correctly_chained_entries() {
        let origin = Ledger::open(":memory:").unwrap();
        let replica = Ledger::open(":memory:").unwrap();
        let e1 = origin.append("artifact", r#"{"id":"a"}"#, 1).unwrap();
        let e2 = origin.append("artifact", r#"{"id":"b"}"#, 2).unwrap();
        replica.append_verified(&e1).unwrap();
        replica.append_verified(&e2).unwrap();
        assert_eq!(replica.verify_chain().unwrap(), 2);
        assert_eq!(replica.head_hash().unwrap(), origin.head_hash().unwrap());

        // A forged entry that doesn't chain is refused.
        let mut forged = e2.clone();
        forged.body = r#"{"id":"evil"}"#.into();
        assert!(replica.append_verified(&forged).is_err());
    }
}
