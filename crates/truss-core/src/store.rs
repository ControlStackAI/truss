//! Durable store — SQLite-backed journal, snapshots, and tool memoization.
//!
//! This is the M1 (Stage C) substrate from `docs/durable-execution.md`: an
//! append-only, hash-chained **journal** (the source of truth), plus **snapshots**
//! (message history + structured state, for fast resume) and a **tool memo**
//! (side-effect ledger, so a resumed run never re-executes a completed tool).
//!
//! Entries are hash-chained **and** ed25519-signed (M2): tamper-evident *and*
//! non-repudiable. The schema matches the design doc so Stage A (positional
//! replay) reads the same tables with no migration.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::approval::APPROVAL_GATE_SIGNAL;
use crate::identity::{verify_sig, Signer};

pub const CHILD_ANCHOR_KIND: &str = "child_anchor";
pub const FANOUT_REDUCTION_KIND: &str = "fanout_reduction";
pub const HUMAN_REVIEW_KIND: &str = "human_review";
pub const APPROVAL_GATE_FIELD: &str = "approval_gate";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
  id           TEXT PRIMARY KEY,
  status       TEXT NOT NULL,
  model        TEXT,
  tenant       TEXT NOT NULL DEFAULT 'local',
  user         TEXT NOT NULL DEFAULT 'local',
  head_hash    BLOB,
  pubkey_id    TEXT,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS journal (
  session_id   TEXT NOT NULL,
  seq          INTEGER NOT NULL,
  kind         TEXT NOT NULL,
  payload      TEXT NOT NULL,
  prev_hash    BLOB NOT NULL,
  entry_hash   BLOB NOT NULL,
  signature    BLOB,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (session_id, seq)
);
CREATE TABLE IF NOT EXISTS snapshots (
  session_id   TEXT NOT NULL,
  at_seq       INTEGER NOT NULL,
  messages     TEXT NOT NULL,
  state        TEXT NOT NULL,
  PRIMARY KEY (session_id, at_seq)
);
CREATE TABLE IF NOT EXISTS tool_memo (
  session_id   TEXT NOT NULL,
  call_key     TEXT NOT NULL,
  content      TEXT NOT NULL,
  is_error     INTEGER NOT NULL,
  PRIMARY KEY (session_id, call_key)
);
CREATE TABLE IF NOT EXISTS signals (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id   TEXT NOT NULL,
  name         TEXT NOT NULL,
  payload      TEXT,
  status       TEXT NOT NULL,
  created_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS anchors (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id   TEXT NOT NULL,
  log          TEXT NOT NULL,
  log_index    INTEGER NOT NULL,
  head_hash    TEXT NOT NULL,
  entry_hash   TEXT NOT NULL,
  anchored_at  INTEGER NOT NULL
);
"#;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub status: String,
    pub model: Option<String>,
    pub head_hash: Option<Vec<u8>>,
    pub pubkey_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionListRow {
    pub id: String,
    pub status: String,
    pub model: Option<String>,
    pub created_at: i64,
    pub head_hash: Option<Vec<u8>>,
    pub entry_count: usize,
}

#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub seq: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub prev_hash: Vec<u8>,
    pub entry_hash: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanReviewDecision {
    Approved,
    Rejected,
    NeedsChanges,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanReviewEntry {
    pub decision: HumanReviewDecision,
    pub note: Option<String>,
    pub attested_head_hash: String,
    pub reviewer_key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalGate {
    pub signal_name: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct AnchorRow {
    pub log: String,
    pub log_index: u64,
    pub head_hash: String,
    pub entry_hash: String,
    pub anchored_at: i64,
}

/// Canonical bytes for hashing: serde_json sorts object keys by default (no
/// `preserve_order` feature), so this is stable across runs.
fn canon(payload: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(payload).unwrap_or_default()
}

fn hash_entry(prev: &[u8], kind: &str, payload: &serde_json::Value, seq: i64) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(kind.as_bytes());
    h.update(canon(payload));
    h.update(seq.to_le_bytes());
    h.finalize().to_vec()
}

pub fn is_approval_gate_payload(payload: &serde_json::Value) -> bool {
    payload
        .get(APPROVAL_GATE_FIELD)
        .map(|gate| !gate.is_null())
        .unwrap_or(false)
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    signer: Arc<Signer>,
    pubkey: String,
}

/// Idempotently add columns introduced after M1, so an existing harness.db keeps
/// working. Duplicate-column errors are ignored.
fn migrate(conn: &Connection) {
    let _ = conn.execute("ALTER TABLE journal ADD COLUMN signature BLOB", []);
    let _ = conn.execute("ALTER TABLE sessions ADD COLUMN pubkey_id TEXT", []);
}

impl Store {
    pub fn open(path: &str, signer: Arc<Signer>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn);
        let pubkey = signer.public_hex();
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            signer,
            pubkey,
        })
    }

    pub fn open_readonly(path: &str) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let signer = Arc::new(Signer::generate());
        let pubkey = signer.public_hex();
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            signer,
            pubkey,
        })
    }

    /// In-memory store with an ephemeral signing key, for tests.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        let signer = Arc::new(Signer::generate());
        let pubkey = signer.public_hex();
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            signer,
            pubkey,
        })
    }

    pub fn create_session(
        &self,
        id: &str,
        model: Option<&str>,
        tenant: &str,
        user: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let t = now_ms();
        conn.execute(
            "INSERT INTO sessions (id, status, model, tenant, user, head_hash, pubkey_id, created_at, updated_at)
             VALUES (?1, 'running', ?2, ?3, ?4, NULL, ?5, ?6, ?6)",
            params![id, model, tenant, user, self.pubkey, t],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, status, model, head_hash, pubkey_id FROM sessions WHERE id = ?1",
                params![id],
                |r| {
                    Ok(SessionRow {
                        id: r.get(0)?,
                        status: r.get(1)?,
                        model: r.get(2)?,
                        head_hash: r.get(3)?,
                        pubkey_id: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionListRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.status, s.model, s.created_at, s.head_hash, COUNT(j.seq)
             FROM sessions s
             LEFT JOIN journal j ON j.session_id = s.id
             GROUP BY s.id, s.status, s.model, s.created_at, s.head_hash
             ORDER BY s.created_at DESC, s.id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let entry_count: i64 = r.get(5)?;
            Ok(SessionListRow {
                id: r.get(0)?,
                status: r.get(1)?,
                model: r.get(2)?,
                created_at: r.get(3)?,
                head_hash: r.get(4)?,
                entry_count: entry_count.max(0) as usize,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status, now_ms()],
        )?;
        Ok(())
    }

    /// Append a journal entry, extending the hash chain. Atomic per call.
    pub fn append(
        &self,
        id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<JournalEntry> {
        let conn = self.conn.lock().unwrap();
        let (next_seq, prev_hash): (i64, Vec<u8>) = {
            let max: Option<i64> = conn
                .query_row(
                    "SELECT MAX(seq) FROM journal WHERE session_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            match max {
                Some(m) => {
                    let prev: Vec<u8> = conn.query_row(
                        "SELECT entry_hash FROM journal WHERE session_id = ?1 AND seq = ?2",
                        params![id, m],
                        |r| r.get(0),
                    )?;
                    (m + 1, prev)
                }
                None => (0, vec![0u8; 32]),
            }
        };
        let entry_hash = hash_entry(&prev_hash, kind, payload, next_seq);
        let signature = self.signer.sign(&entry_hash);
        let t = now_ms();
        conn.execute(
            "INSERT INTO journal (session_id, seq, kind, payload, prev_hash, entry_hash, signature, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, next_seq, kind, payload.to_string(), prev_hash, entry_hash, signature, t],
        )?;
        conn.execute(
            "UPDATE sessions SET head_hash = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, entry_hash, t],
        )?;
        Ok(JournalEntry {
            seq: next_seq,
            kind: kind.to_string(),
            payload: payload.clone(),
            prev_hash,
            entry_hash,
            signature,
        })
    }

    pub fn entries(&self, id: &str) -> Result<Vec<JournalEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, kind, payload, prev_hash, entry_hash, signature FROM journal
             WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            let payload: String = r.get(2)?;
            let signature: Option<Vec<u8>> = r.get(5)?;
            Ok(JournalEntry {
                seq: r.get(0)?,
                kind: r.get(1)?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                prev_hash: r.get(3)?,
                entry_hash: r.get(4)?,
                signature: signature.unwrap_or_default(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn last_entry(&self, id: &str) -> Result<Option<JournalEntry>> {
        Ok(self.entries(id)?.pop())
    }

    pub fn head_hash_hex(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .last_entry(id)?
            .map(|entry| hex::encode(entry.entry_hash)))
    }

    pub fn record_human_review(
        &self,
        id: &str,
        decision: HumanReviewDecision,
        note: Option<String>,
    ) -> Result<JournalEntry> {
        if self.get_session(id)?.is_none() {
            return Err(anyhow::anyhow!("session {id} not found"));
        }
        let attested_head_hash = self
            .head_hash_hex(id)?
            .ok_or_else(|| anyhow::anyhow!("session {id} has no head hash to attest"))?;
        let payload = serde_json::to_value(HumanReviewEntry {
            decision,
            note,
            attested_head_hash,
            reviewer_key_id: self.pubkey.clone(),
        })?;
        let entry = self.append(id, HUMAN_REVIEW_KIND, &payload)?;
        if decision == HumanReviewDecision::Approved {
            if let Some(gate) = self.pending_approval_gate(id)? {
                let release_payload = serde_json::json!({
                    "released_by": HUMAN_REVIEW_KIND,
                    "review_seq": entry.seq,
                    "review_entry_hash": hex::encode(&entry.entry_hash),
                    "decision": "approved",
                });
                self.add_signal(id, &gate.signal_name, Some(&release_payload.to_string()))?;
            }
        }
        Ok(entry)
    }

    pub fn pending_approval_gate(&self, id: &str) -> Result<Option<PendingApprovalGate>> {
        let mut pending = None;
        for entry in self.entries(id)? {
            match entry.kind.as_str() {
                "suspended" if is_approval_gate_payload(&entry.payload) => {
                    pending = Some(PendingApprovalGate {
                        signal_name: entry
                            .payload
                            .get("signal_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(APPROVAL_GATE_SIGNAL)
                            .to_string(),
                        reason: entry
                            .payload
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    });
                }
                "signal_received"
                    if entry.payload.get("name").and_then(|v| v.as_str())
                        == Some(APPROVAL_GATE_SIGNAL) =>
                {
                    pending = None;
                }
                "run_completed" | "run_failed" => pending = None,
                _ => {}
            }
        }
        Ok(pending)
    }

    pub fn root_hash_hex(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .entries(id)?
            .into_iter()
            .next()
            .map(|entry| hex::encode(entry.entry_hash)))
    }

    pub fn save_snapshot(&self, id: &str, at_seq: i64, messages: &str, state: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO snapshots (session_id, at_seq, messages, state)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, at_seq, messages, state],
        )?;
        Ok(())
    }

    pub fn latest_snapshot(&self, id: &str) -> Result<Option<(i64, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT at_seq, messages, state FROM snapshots
                 WHERE session_id = ?1 ORDER BY at_seq DESC LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row)
    }

    pub fn memo_get(&self, id: &str, call_key: &str) -> Result<Option<(String, bool)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT content, is_error FROM tool_memo WHERE session_id = ?1 AND call_key = ?2",
                params![id, call_key],
                |r| {
                    let content: String = r.get(0)?;
                    let is_error: i64 = r.get(1)?;
                    Ok((content, is_error != 0))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn memo_put(&self, id: &str, call_key: &str, content: &str, is_error: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tool_memo (session_id, call_key, content, is_error)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, call_key, content, is_error as i64],
        )?;
        Ok(())
    }

    /// Append a signed journal entry and write the matching tool memo in one
    /// SQLite transaction. Used for external calls where the memo is the
    /// no-reinvoke replay path, but the journal remains the audit source.
    pub fn append_with_memo(
        &self,
        id: &str,
        kind: &str,
        payload: &serde_json::Value,
        call_key: &str,
        content: &str,
        is_error: bool,
    ) -> Result<JournalEntry> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (next_seq, prev_hash): (i64, Vec<u8>) = {
            let max: Option<i64> = tx
                .query_row(
                    "SELECT MAX(seq) FROM journal WHERE session_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            match max {
                Some(m) => {
                    let prev: Vec<u8> = tx.query_row(
                        "SELECT entry_hash FROM journal WHERE session_id = ?1 AND seq = ?2",
                        params![id, m],
                        |r| r.get(0),
                    )?;
                    (m + 1, prev)
                }
                None => (0, vec![0u8; 32]),
            }
        };
        let entry_hash = hash_entry(&prev_hash, kind, payload, next_seq);
        let signature = self.signer.sign(&entry_hash);
        let t = now_ms();
        tx.execute(
            "INSERT INTO journal (session_id, seq, kind, payload, prev_hash, entry_hash, signature, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, next_seq, kind, payload.to_string(), prev_hash, entry_hash, signature, t],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO tool_memo (session_id, call_key, content, is_error)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, call_key, content, is_error as i64],
        )?;
        tx.execute(
            "UPDATE sessions SET head_hash = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, entry_hash, t],
        )?;
        tx.commit()?;
        Ok(JournalEntry {
            seq: next_seq,
            kind: kind.to_string(),
            payload: payload.clone(),
            prev_hash,
            entry_hash,
            signature,
        })
    }

    /// Durably record an external signal for a session (status `pending`).
    pub fn add_signal(&self, id: &str, name: &str, payload: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO signals (session_id, name, payload, status, created_at)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
            params![id, name, payload, now_ms()],
        )?;
        Ok(())
    }

    /// Consume the oldest pending signal with `name` for a session, marking it
    /// delivered. Returns its payload (empty string if none), or `None` if no
    /// pending signal matches.
    pub fn take_pending_signal(&self, id: &str, name: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT id, payload FROM signals
                 WHERE session_id = ?1 AND name = ?2 AND status = 'pending'
                 ORDER BY id ASC LIMIT 1",
                params![id, name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((sig_id, payload)) => {
                conn.execute(
                    "UPDATE signals SET status = 'delivered' WHERE id = ?1",
                    params![sig_id],
                )?;
                Ok(Some(payload.unwrap_or_default()))
            }
            None => Ok(None),
        }
    }

    /// Record an external anchor receipt for a session.
    pub fn record_anchor(
        &self,
        id: &str,
        log: &str,
        log_index: u64,
        head_hash_hex: &str,
        entry_hash_hex: &str,
        anchored_at: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO anchors (session_id, log, log_index, head_hash, entry_hash, anchored_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                log,
                log_index as i64,
                head_hash_hex,
                entry_hash_hex,
                anchored_at
            ],
        )?;
        Ok(())
    }

    /// Number of times a session has been anchored.
    pub fn anchor_count(&self, id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM anchors WHERE session_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// External anchor receipts recorded for a session.
    pub fn anchors(&self, id: &str) -> Result<Vec<AnchorRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT log, log_index, head_hash, entry_hash, anchored_at FROM anchors
             WHERE session_id = ?1 ORDER BY log_index ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            let log_index: i64 = r.get(1)?;
            Ok(AnchorRow {
                log: r.get(0)?,
                log_index: log_index.max(0) as u64,
                head_hash: r.get(2)?,
                entry_hash: r.get(3)?,
                anchored_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All session ids currently in `suspended` status (for the resume daemon).
    pub fn list_suspended_sessions(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id FROM sessions WHERE status = 'suspended'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Test-only: overwrite a journal payload WITHOUT re-chaining, to simulate
    /// tampering. Kept compiled in release so integration tests can run under
    /// `cargo test --release`.
    #[doc(hidden)]
    pub fn raw_set_payload_for_test(
        &self,
        id: &str,
        seq: i64,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE journal SET payload = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![id, seq, payload.to_string()],
        )?;
        Ok(())
    }

    /// Test-only: corrupt a journal signature in place.
    #[doc(hidden)]
    pub fn raw_corrupt_signature_for_test(&self, id: &str, seq: i64) -> Result<()> {
        self.raw_flip_signature_byte_for_test(id, seq)
    }

    /// Test-only: corrupt a stored journal entry hash in place.
    #[doc(hidden)]
    pub fn raw_corrupt_entry_hash_for_test(&self, id: &str, seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut hash: Vec<u8> = conn.query_row(
            "SELECT entry_hash FROM journal WHERE session_id = ?1 AND seq = ?2",
            params![id, seq],
            |r| r.get(0),
        )?;
        if let Some(first) = hash.first_mut() {
            *first ^= 0x01;
        }
        conn.execute(
            "UPDATE journal SET entry_hash = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![id, seq, hash],
        )?;
        Ok(())
    }

    /// Test-only: flip a single byte of a journal signature in place.
    #[doc(hidden)]
    pub fn raw_flip_signature_byte_for_test(&self, id: &str, seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut signature: Vec<u8> = conn.query_row(
            "SELECT signature FROM journal WHERE session_id = ?1 AND seq = ?2",
            params![id, seq],
            |r| r.get(0),
        )?;
        if let Some(first) = signature.first_mut() {
            *first ^= 0x01;
        }
        conn.execute(
            "UPDATE journal SET signature = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![id, seq, signature],
        )?;
        Ok(())
    }

    /// Test-only: swap two journal rows' sequence numbers without re-chaining.
    #[doc(hidden)]
    pub fn raw_swap_entry_order_for_test(
        &self,
        id: &str,
        left_seq: i64,
        right_seq: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let temp_seq = -9_000_000_000_000_i64 + left_seq;
        conn.execute(
            "UPDATE journal SET seq = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![id, left_seq, temp_seq],
        )?;
        conn.execute(
            "UPDATE journal SET seq = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![id, right_seq, left_seq],
        )?;
        conn.execute(
            "UPDATE journal SET seq = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![id, temp_seq, right_seq],
        )?;
        Ok(())
    }

    /// Test-only: replay/duplicate a journal row at the current tail without
    /// updating the copied hash material.
    #[doc(hidden)]
    pub fn raw_duplicate_entry_for_test(&self, id: &str, seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let next_seq = conn
            .query_row(
                "SELECT MAX(seq) FROM journal WHERE session_id = ?1",
                params![id],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(-1)
            + 1;
        let (kind, payload, prev_hash, entry_hash, signature, created_at): (
            String,
            String,
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            i64,
        ) = conn.query_row(
            "SELECT kind, payload, prev_hash, entry_hash, signature, created_at
             FROM journal WHERE session_id = ?1 AND seq = ?2",
            params![id, seq],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        conn.execute(
            "INSERT INTO journal (session_id, seq, kind, payload, prev_hash, entry_hash, signature, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, next_seq, kind, payload, prev_hash, entry_hash, signature, created_at],
        )?;
        Ok(())
    }

    /// Test-only: delete one journal row without updating the session head.
    #[doc(hidden)]
    pub fn raw_delete_entry_for_test(&self, id: &str, seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM journal WHERE session_id = ?1 AND seq = ?2",
            params![id, seq],
        )?;
        Ok(())
    }

    /// Test-only: replace one payload and recompute hashes/signatures from that
    /// point forward. This simulates a key-holder rewriting signed history so
    /// semantic reference checks are exercised separately from hash checks.
    #[doc(hidden)]
    pub fn raw_replace_payload_and_rechain_for_test(
        &self,
        id: &str,
        seq: i64,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut prev = if seq == 0 {
            vec![0u8; 32]
        } else {
            tx.query_row(
                "SELECT entry_hash FROM journal WHERE session_id = ?1 AND seq = ?2",
                params![id, seq - 1],
                |r| r.get(0),
            )?
        };
        let rows: Vec<(i64, String, serde_json::Value)> = {
            let mut stmt = tx.prepare(
                "SELECT seq, kind, payload FROM journal
                 WHERE session_id = ?1 AND seq >= ?2 ORDER BY seq ASC",
            )?;
            let mapped = stmt.query_map(params![id, seq], |r| {
                let row_seq: i64 = r.get(0)?;
                let kind: String = r.get(1)?;
                let payload_text: String = r.get(2)?;
                let payload_value = if row_seq == seq {
                    payload.clone()
                } else {
                    serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null)
                };
                Ok((row_seq, kind, payload_value))
            })?;
            let mut rows = Vec::new();
            for row in mapped {
                rows.push(row?);
            }
            rows
        };
        if rows.is_empty() {
            return Ok(());
        }

        for (row_seq, kind, payload_value) in rows {
            let entry_hash = hash_entry(&prev, &kind, &payload_value, row_seq);
            let signature = self.signer.sign(&entry_hash);
            tx.execute(
                "UPDATE journal
                 SET payload = ?3, prev_hash = ?4, entry_hash = ?5, signature = ?6
                 WHERE session_id = ?1 AND seq = ?2",
                params![
                    id,
                    row_seq,
                    payload_value.to_string(),
                    prev,
                    entry_hash,
                    signature
                ],
            )?;
            prev = entry_hash;
        }
        tx.execute(
            "UPDATE sessions SET head_hash = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, prev, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Verify the hash chain **and** signatures end-to-end. Returns the broken
    /// seq on failure. Signatures are checked against the session's recorded
    /// `pubkey_id`; if `expect_pubkey` is given, it must also match (out-of-band
    /// pinning that defends against a key-holding tamperer).
    pub fn verify_chain(&self, id: &str) -> Result<Result<usize, i64>> {
        self.verify_chain_pinned(id, None)
    }

    pub fn verify_chain_pinned(
        &self,
        id: &str,
        expect_pubkey: Option<&str>,
    ) -> Result<Result<usize, i64>> {
        self.verify_chain_recursive(id, expect_pubkey, &mut HashSet::new())
    }

    fn verify_chain_recursive(
        &self,
        id: &str,
        expect_pubkey: Option<&str>,
        visited: &mut HashSet<String>,
    ) -> Result<Result<usize, i64>> {
        if !visited.insert(id.to_string()) {
            return Ok(Ok(0));
        }
        let session = self.get_session(id)?;
        let pubkey = session.as_ref().and_then(|s| s.pubkey_id.clone());
        if let (Some(expect), Some(actual)) = (expect_pubkey, pubkey.as_deref()) {
            if expect != actual {
                return Ok(Err(0)); // pinned key mismatch
            }
        }

        let entries = self.entries(id)?;
        let mut prev = vec![0u8; 32];
        for e in &entries {
            if e.prev_hash != prev {
                return Ok(Err(e.seq));
            }
            let expect_hash = hash_entry(&prev, &e.kind, &e.payload, e.seq);
            if expect_hash != e.entry_hash {
                return Ok(Err(e.seq));
            }
            // Signatures: normal run entries use the session key; human review
            // entries are signed by the reviewer key they carry in payload.
            if e.kind == HUMAN_REVIEW_KIND {
                let Some(reviewer_key_id) =
                    e.payload.get("reviewer_key_id").and_then(|v| v.as_str())
                else {
                    return Ok(Err(e.seq));
                };
                if !verify_sig(reviewer_key_id, &e.entry_hash, &e.signature) {
                    return Ok(Err(e.seq));
                }
            } else if let Some(pk) = &pubkey {
                if !verify_sig(pk, &e.entry_hash, &e.signature) {
                    return Ok(Err(e.seq));
                }
            }
            if e.kind == CHILD_ANCHOR_KIND
                && !self.verify_child_anchor_payload(&e.payload, visited)?
            {
                return Ok(Err(e.seq));
            }
            if e.kind == FANOUT_REDUCTION_KIND
                && !self.verify_fanout_reduction_payload(id, &e.payload)?
            {
                return Ok(Err(e.seq));
            }
            prev = e.entry_hash.clone();
        }
        if let Some(session) = session {
            match (session.head_hash.as_deref(), entries.last()) {
                (Some(head), Some(last)) if head != last.entry_hash.as_slice() => {
                    return Ok(Err(last.seq));
                }
                (Some(_), None) => return Ok(Err(0)),
                _ => {}
            }
        }
        Ok(Ok(entries.len()))
    }

    fn verify_child_anchor_payload(
        &self,
        payload: &serde_json::Value,
        visited: &mut HashSet<String>,
    ) -> Result<bool> {
        let Some(child_session) = payload.get("child_run_id").and_then(|v| v.as_str()) else {
            return Ok(false);
        };
        let Some(expected_head) = payload.get("child_head_hash").and_then(|v| v.as_str()) else {
            return Ok(false);
        };
        if self
            .verify_chain_recursive(child_session, None, visited)?
            .is_err()
        {
            return Ok(false);
        }
        if self.head_hash_hex(child_session)?.as_deref() != Some(expected_head) {
            return Ok(false);
        }
        if let Some(expected_root) = payload.get("child_root_hash").and_then(|v| v.as_str()) {
            if self.root_hash_hex(child_session)?.as_deref() != Some(expected_root) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn verify_fanout_reduction_payload(
        &self,
        id: &str,
        payload: &serde_json::Value,
    ) -> Result<bool> {
        if payload.get("parent_run_id").and_then(|v| v.as_str()) != Some(id) {
            return Ok(false);
        }
        let Some(children) = payload.get("children").and_then(|v| v.as_array()) else {
            return Ok(false);
        };
        if children.is_empty() {
            return Ok(false);
        }
        let entries = self.entries(id)?;
        let mut child_ids = HashSet::new();
        for child in children {
            let Some(child_run_id) = child.get("child_run_id").and_then(|v| v.as_str()) else {
                return Ok(false);
            };
            let Some(status) = child.get("status").and_then(|v| v.as_str()) else {
                return Ok(false);
            };
            let Some(anchor_seq) = child
                .get("parent_anchor")
                .and_then(|v| v.get("seq"))
                .and_then(|v| v.as_i64())
            else {
                return Ok(false);
            };
            let Some(anchor) = entries
                .iter()
                .find(|entry| entry.seq == anchor_seq && entry.kind == CHILD_ANCHOR_KIND)
            else {
                return Ok(false);
            };
            if anchor.payload.get("child_run_id").and_then(|v| v.as_str()) != Some(child_run_id) {
                return Ok(false);
            }
            if anchor.payload.get("status").and_then(|v| v.as_str()) != Some(status) {
                return Ok(false);
            }
            if child.get("child_head_hash").and_then(|v| v.as_str())
                != anchor
                    .payload
                    .get("child_head_hash")
                    .and_then(|v| v.as_str())
            {
                return Ok(false);
            }
            let child_verdict = child.get("verdict_ref");
            let anchor_verdict = anchor.payload.get("verdict_ref");
            if !ledger_ref_values_match(child_verdict, anchor_verdict) {
                return Ok(false);
            }
            child_ids.insert(child_run_id.to_string());
        }

        if let Some(winner) = payload.get("winner") {
            if winner.is_null() {
                return Ok(true);
            }
            let Some(winner_child) = winner.get("child_run_id").and_then(|v| v.as_str()) else {
                return Ok(false);
            };
            if !child_ids.contains(winner_child) {
                return Ok(false);
            }
            let Some(winner_verdict) = winner.get("verdict_ref") else {
                return Ok(false);
            };
            let Some(matching_child) = children.iter().find(|child| {
                child.get("child_run_id").and_then(|v| v.as_str()) == Some(winner_child)
            }) else {
                return Ok(false);
            };
            if matching_child.get("status").and_then(|v| v.as_str()) != Some("completed") {
                return Ok(false);
            }
            if !ledger_ref_values_match(Some(winner_verdict), matching_child.get("verdict_ref")) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The signing identity's public key (hex).
    pub fn pubkey(&self) -> &str {
        &self.pubkey
    }

    /// Share the durable SQLite connection with adjacent substrates that need to
    /// live in the same signed database.
    pub(crate) fn connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }
}

/// Deterministic memo key for a tool call (content-addressed). M5/Stage A will
/// switch to positional keys for exact replay; for Stage C this dedups a
/// re-issued call after a crash.
pub fn tool_call_key(name: &str, input: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(canon(input));
    format!("{:x}", h.finalize())
}

fn ledger_ref_values_match(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(l), Some(r)) if l.is_null() && r.is_null() => true,
        (Some(l), Some(r)) => {
            l.get("session").and_then(|v| v.as_str()) == r.get("session").and_then(|v| v.as_str())
                && l.get("seq").and_then(|v| v.as_i64()) == r.get("seq").and_then(|v| v.as_i64())
        }
        _ => false,
    }
}
