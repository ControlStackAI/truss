//! The substrate-agnostic memory store trait.
//!
//! Like [`crate::provider::Provider`] and [`crate::executor`], the *interface*
//! is fixed and the *backing* is swappable: M1 ships a SQLite impl that shares
//! the durable-execution database ([`crate::store`]); M5 can add a vector index
//! behind the same trait with no agent-loop change.
//!
//! Sync (not async) to match the rusqlite-backed [`crate::store::Store`] —
//! memory writes are local disk, not network.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::embed::Embedder;
use super::policy::MemoryRecallMode;
use super::types::{
    EntryKind, LedgerRef, MemoryCursor, MemoryDelta, MemoryDraft, MemoryEntry, MemoryId,
    MemoryQuery, PolicyDraft, PolicyId, PolicyRecord, Provenance, Trust,
};
use crate::delegation::Verdict;
use crate::store::Store;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory entry not found: {0:?}")]
    NotFound(MemoryId),
    #[error("supersede key conflict: {0}")]
    KeyConflict(String),
    #[error("store backend error: {0}")]
    Backend(#[from] anyhow::Error),
    #[error("semantic memory recall requires an embedder")]
    MissingEmbedder,
    #[error(
        "{count} memory entr{suffix} match this semantic query but lack embeddings; run `truss memory reindex`"
    )]
    MissingEmbeddings { count: usize, suffix: &'static str },
    #[error("memory recall mode '{0}' is reserved for S2")]
    UnsupportedRecallMode(&'static str),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Truss's governed memory substrate. The model never calls this directly — the
/// `memory_20250818` tool binding (M2) maps tool calls onto these methods, and
/// the verification pass (M3) drives `promote`/`reject`/`mark_stale`.
pub trait MemoryStore: Send + Sync {
    /// Land a model write. **Always enters `Unverified`** — promotion is a
    /// separate, gated step. Returns the content-addressed id.
    fn write(&self, draft: MemoryDraft) -> Result<MemoryId, MemoryError>;

    /// Promote `Unverified → Verified`, attaching the signed N-reviewer verdict
    /// as provenance. Idempotent on an already-verified entry.
    fn promote(&self, id: &MemoryId, verdict: Verdict) -> Result<(), MemoryError>;

    /// Promote with the durable ledger reference that recorded the verifier
    /// verdict. Stores that cannot persist the reference may fall back to
    /// [`MemoryStore::promote`].
    fn promote_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: Verdict,
        _verdict_ref: LedgerRef,
    ) -> Result<(), MemoryError> {
        self.promote(id, verdict)
    }

    /// Mark a refuted entry `Rejected` (retained for audit, never surfaced).
    fn reject(&self, id: &MemoryId, verdict: Verdict) -> Result<(), MemoryError>;

    /// Reject with the durable ledger reference that recorded the verifier
    /// verdict. Stores that cannot persist the reference may fall back to
    /// [`MemoryStore::reject`].
    fn reject_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: Verdict,
        _verdict_ref: LedgerRef,
    ) -> Result<(), MemoryError> {
        self.reject(id, verdict)
    }

    /// Demote a previously-verified entry to `Stale` (contradiction / failed
    /// re-check), linking the entry that superseded it if any.
    fn mark_stale(&self, id: &MemoryId, superseded_by: Option<MemoryId>)
        -> Result<(), MemoryError>;

    /// Fetch one entry by id (any trust tier).
    fn get(&self, id: &MemoryId) -> Result<MemoryEntry, MemoryError>;

    /// Query memory. Defaults (via [`MemoryQuery::default`]) to `Verified`-only —
    /// this backs the model's `view`, i.e. pull-trusted-on-demand.
    fn query(&self, q: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError>;

    /// Everything that changed since `cursor` — for delta rehydration on resume.
    fn delta_since(&self, cursor: MemoryCursor) -> Result<MemoryDelta, MemoryError>;

    /// Land a governed policy object. **Always enters `Unverified`**; future L3
    /// promotion will reuse the same verdict lane as memory.
    fn write_policy(&self, draft: PolicyDraft) -> Result<PolicyId, MemoryError>;

    /// Fetch one governed policy record by id.
    fn get_policy(&self, id: &PolicyId) -> Result<PolicyRecord, MemoryError>;
}

// ── M1 backing impl (skeleton) ───────────────────────────────────────────────
//
// SqliteMemoryStore reuses the durable-execution connection so memory entries
// and the journal share one signed database (provenance `LedgerRef` already
// points at `journal(session_id, seq)`). Schema sketch:
//
//   CREATE TABLE memory (
//     id            TEXT PRIMARY KEY,   -- content-addressed
//     kind          TEXT NOT NULL,
//     mem_key       TEXT,               -- supersede key (Fact/Lesson)
//     body          TEXT NOT NULL,
//     trust         TEXT NOT NULL,      -- unverified|verified|rejected|stale
//     produced_sess TEXT NOT NULL,      -- provenance: journal ref
//     produced_seq  INTEGER NOT NULL,
//     verdict_json  TEXT,               -- signed Verdict iff verified
//     salience      REAL NOT NULL DEFAULT 0.0,
//     superseded_by TEXT,
//     created_at    INTEGER NOT NULL,
//     write_seq     INTEGER NOT NULL    -- monotonic, drives delta_since cursor
//   );
//
// `write_seq` is a per-store monotonic counter so `delta_since` is a cheap
// `WHERE write_seq > ?`. Promotion/rejection update `trust` + `verdict_json`
// in place and bump `write_seq` so consumers see the tier change in their delta.
//
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memory (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  mem_key       TEXT,
  body          TEXT NOT NULL,
  trust         TEXT NOT NULL,
  produced_sess TEXT NOT NULL,
  produced_seq  INTEGER NOT NULL,
  sources_json  TEXT NOT NULL DEFAULT '[]',
  verdict_json  TEXT,
  verdict_sess  TEXT,
  verdict_seq   INTEGER,
  salience      REAL NOT NULL DEFAULT 0.0,
  superseded_by TEXT,
  created_at    INTEGER NOT NULL,
  write_seq     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_memory_key_trust ON memory(mem_key, trust);
CREATE INDEX IF NOT EXISTS idx_memory_write_seq ON memory(write_seq);
CREATE TABLE IF NOT EXISTS memory_embeddings (
  memory_id   TEXT PRIMARY KEY,
  embedder_id TEXT NOT NULL,
  dims        INTEGER NOT NULL,
  vector_json TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  FOREIGN KEY(memory_id) REFERENCES memory(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_memory_embeddings_model ON memory_embeddings(embedder_id, dims);
CREATE TABLE IF NOT EXISTS policy_records (
  id            TEXT PRIMARY KEY,
  policy_json   TEXT NOT NULL,
  trust         TEXT NOT NULL,
  produced_sess TEXT NOT NULL,
  produced_seq  INTEGER NOT NULL,
  sources_json  TEXT NOT NULL DEFAULT '[]',
  verdict_json  TEXT,
  superseded_by TEXT,
  created_at    INTEGER NOT NULL,
  write_seq     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_policy_records_write_seq ON policy_records(write_seq);
"#;

/// SQLite-backed M1 memory substrate.
#[derive(Clone)]
pub struct SqliteMemoryStore {
    conn: Arc<Mutex<Connection>>,
    options: MemoryStoreOptions,
}

/// SQLite memory-store runtime options. Exact recall has no embedder
/// requirement; semantic recall embeds writes and vector-ranks `contains`.
#[derive(Clone)]
pub struct MemoryStoreOptions {
    pub recall: MemoryRecallMode,
    pub embedder: Option<Arc<dyn Embedder>>,
}

impl Default for MemoryStoreOptions {
    fn default() -> Self {
        Self {
            recall: MemoryRecallMode::Exact,
            embedder: None,
        }
    }
}

impl MemoryStoreOptions {
    pub fn semantic(embedder: Arc<dyn Embedder>) -> Self {
        Self {
            recall: MemoryRecallMode::Semantic,
            embedder: Some(embedder),
        }
    }

    pub fn hybrid(embedder: Arc<dyn Embedder>) -> Self {
        Self {
            recall: MemoryRecallMode::Hybrid,
            embedder: Some(embedder),
        }
    }
}

/// Stored embedding metadata for audit/debugging and migration checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEmbeddingMetadata {
    pub memory_id: MemoryId,
    pub embedder_id: String,
    pub dims: usize,
}

/// Summary returned by explicit embedding backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReindexSummary {
    pub scanned: usize,
    pub embedded: usize,
}

impl SqliteMemoryStore {
    /// Open a standalone SQLite database for memory. Production callers should
    /// prefer [`SqliteMemoryStore::from_store`] so memory and the durable journal
    /// live in the same DB file.
    pub fn open(path: &str) -> Result<Self, MemoryError> {
        let conn = Connection::open(path)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            options: MemoryStoreOptions::default(),
        })
    }

    pub fn open_with_options(path: &str, options: MemoryStoreOptions) -> Result<Self, MemoryError> {
        validate_options(&options)?;
        let conn = Connection::open(path)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            options,
        })
    }

    /// In-memory store for hermetic tests.
    pub fn open_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            options: MemoryStoreOptions::default(),
        })
    }

    pub fn open_memory_with_options(options: MemoryStoreOptions) -> Result<Self, MemoryError> {
        validate_options(&options)?;
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            options,
        })
    }

    /// Reuse the durable-execution store's SQLite connection.
    pub fn from_store(store: &Store) -> Result<Self, MemoryError> {
        let conn = store.connection();
        {
            let conn = conn.lock().unwrap();
            init_schema(&conn)?;
        }
        Ok(Self {
            conn,
            options: MemoryStoreOptions::default(),
        })
    }

    pub fn from_store_with_options(
        store: &Store,
        options: MemoryStoreOptions,
    ) -> Result<Self, MemoryError> {
        validate_options(&options)?;
        let conn = store.connection();
        {
            let conn = conn.lock().unwrap();
            init_schema(&conn)?;
        }
        Ok(Self { conn, options })
    }

    /// Reuse an existing durable store connection without initializing memory
    /// schema. This is for read-only ledger views: callers must check that the
    /// memory table exists before issuing memory queries.
    pub(crate) fn from_existing_store(store: &Store) -> Self {
        Self {
            conn: store.connection(),
            options: MemoryStoreOptions::default(),
        }
    }

    pub fn embedding_metadata(
        &self,
        id: &MemoryId,
    ) -> Result<Option<MemoryEmbeddingMetadata>, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let metadata = conn
            .query_row(
                "SELECT memory_id, embedder_id, dims FROM memory_embeddings WHERE memory_id = ?1",
                params![id.0.as_str()],
                |row| {
                    Ok(MemoryEmbeddingMetadata {
                        memory_id: MemoryId(row.get(0)?),
                        embedder_id: row.get(1)?,
                        dims: row.get::<_, i64>(2)? as usize,
                    })
                },
            )
            .optional()?;
        Ok(metadata)
    }

    pub fn reindex_missing_embeddings(&self) -> Result<ReindexSummary, MemoryError> {
        let Some(embedder) = self.options.embedder.as_ref() else {
            return Err(MemoryError::MissingEmbedder);
        };
        let rows = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT memory.id, memory.body
                 FROM memory
                 LEFT JOIN memory_embeddings ON memory_embeddings.memory_id = memory.id
                 WHERE memory_embeddings.memory_id IS NULL
                 ORDER BY memory.write_seq ASC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((MemoryId(row.get(0)?), row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        let scanned = rows.len();
        let mut embedded = 0usize;
        for (id, body) in rows {
            let vector = embedder.embed(&body)?;
            ensure_dims(embedder.as_ref(), &vector)?;
            let conn = self.conn.lock().unwrap();
            persist_embedding(&conn, &id, embedder.as_ref(), &vector)?;
            embedded += 1;
        }
        Ok(ReindexSummary { scanned, embedded })
    }

    fn query_semantic(&self, q: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.query_vector_ranked(q, false)
    }

    fn query_hybrid(&self, q: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.query_vector_ranked(q, true)
    }

    fn query_vector_ranked(
        &self,
        q: &MemoryQuery,
        exact_boost: bool,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let Some(needle) = q.contains.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(embedder) = self.options.embedder.as_ref() else {
            return Err(MemoryError::MissingEmbedder);
        };
        let query_vector = embedder.embed(needle)?;
        ensure_dims(embedder.as_ref(), &query_vector)?;

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, mem_key, body, trust, produced_sess, produced_seq,
                    sources_json, verdict_json, verdict_sess, verdict_seq,
                    salience, superseded_by, created_at, write_seq
             FROM memory
             WHERE superseded_by IS NULL
             ORDER BY salience DESC, write_seq DESC",
        )?;
        let rows = stmt.query_map([], row_to_entry)?;

        let mut missing = 0usize;
        let mut scored = Vec::new();
        for row in rows {
            let (entry, write_seq) = row?;
            // Apply trust/kind/key filters before vector scoring/ranking so the
            // ranking stage never sees entries below the caller's trust floor.
            if !matches_query_filters(&entry, q) {
                continue;
            }
            let exact_match = exact_boost && entry.body.contains(needle);
            let similarity = match load_embedding(&conn, &entry.id, embedder.as_ref())? {
                Some((embedder_id, dims, vector))
                    if embedder_id == embedder.id() && dims == embedder.dims() =>
                {
                    cosine(&query_vector, &vector)
                }
                _ if exact_match => 0.0,
                _ => {
                    missing += 1;
                    continue;
                }
            };
            scored.push((exact_match, similarity, entry.salience, write_seq, entry));
        }

        if missing > 0 {
            return Err(MemoryError::MissingEmbeddings {
                count: missing,
                suffix: if missing == 1 { "y" } else { "ies" },
            });
        }

        scored.sort_by(
            |(a_exact, a_score, a_salience, a_seq, _), (b_exact, b_score, b_salience, b_seq, _)| {
                b_exact.cmp(a_exact).then_with(|| {
                    b_score
                        .partial_cmp(a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| {
                            b_salience
                                .partial_cmp(a_salience)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .then_with(|| b_seq.cmp(a_seq))
                })
            },
        );
        Ok(scored
            .into_iter()
            .take(q.limit)
            .map(|(_, _, _, _, entry)| entry)
            .collect())
    }
}

impl MemoryStore for SqliteMemoryStore {
    fn write(&self, draft: MemoryDraft) -> Result<MemoryId, MemoryError> {
        let id = memory_id(&draft.kind, draft.key.as_deref(), &draft.body);
        let kind = kind_to_str(draft.kind);
        let sources_json = serde_json::to_string(&draft.sources)?;
        let created_at = now_ms();
        let embedding = self
            .options
            .embedder
            .as_ref()
            .map(|embedder| {
                let vector = embedder.embed(&draft.body)?;
                ensure_dims(embedder.as_ref(), &vector)?;
                Ok::<_, MemoryError>((embedder.clone(), vector))
            })
            .transpose()?;

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        if should_supersede(draft.kind, draft.key.as_deref()) {
            if let Some(prior_id) = active_entry_for_key(&tx, draft.kind, draft.key.as_deref())? {
                let seq = next_write_seq(&tx)?;
                tx.execute(
                    "UPDATE memory SET superseded_by = ?2, write_seq = ?3 WHERE id = ?1",
                    params![prior_id.0.as_str(), id.0.as_str(), seq],
                )?;
            }
        }

        let seq = next_write_seq(&tx)?;
        tx.execute(
            "INSERT INTO memory (
               id, kind, mem_key, body, trust, produced_sess, produced_seq,
               sources_json, verdict_json, salience, superseded_by, created_at, write_seq
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, 0.0, NULL, ?9, ?10)",
            params![
                id.0.as_str(),
                kind,
                draft.key.as_deref(),
                draft.body.as_str(),
                trust_to_str(Trust::Unverified),
                draft.produced_by.session.as_str(),
                draft.produced_by.seq,
                sources_json.as_str(),
                created_at,
                seq,
            ],
        )?;
        if let Some((embedder, vector)) = embedding {
            persist_embedding(&tx, &id, embedder.as_ref(), &vector)?;
        }
        tx.commit()?;

        Ok(id)
    }

    fn promote(&self, id: &MemoryId, verdict: Verdict) -> Result<(), MemoryError> {
        update_trust(
            &self.conn,
            id,
            Trust::Verified,
            Some(serde_json::to_string(&verdict)?),
            None,
            None,
            Reconcile::Promoted,
        )
    }

    fn promote_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: Verdict,
        verdict_ref: LedgerRef,
    ) -> Result<(), MemoryError> {
        update_trust(
            &self.conn,
            id,
            Trust::Verified,
            Some(serde_json::to_string(&verdict)?),
            Some(verdict_ref),
            None,
            Reconcile::Promoted,
        )
    }

    fn reject(&self, id: &MemoryId, verdict: Verdict) -> Result<(), MemoryError> {
        update_trust(
            &self.conn,
            id,
            Trust::Rejected,
            Some(serde_json::to_string(&verdict)?),
            None,
            None,
            Reconcile::Rejected,
        )
    }

    fn reject_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: Verdict,
        verdict_ref: LedgerRef,
    ) -> Result<(), MemoryError> {
        update_trust(
            &self.conn,
            id,
            Trust::Rejected,
            Some(serde_json::to_string(&verdict)?),
            Some(verdict_ref),
            None,
            Reconcile::Rejected,
        )
    }

    fn mark_stale(
        &self,
        id: &MemoryId,
        superseded_by: Option<MemoryId>,
    ) -> Result<(), MemoryError> {
        update_trust(
            &self.conn,
            id,
            Trust::Stale,
            None,
            None,
            superseded_by,
            Reconcile::None,
        )
    }

    fn get(&self, id: &MemoryId) -> Result<MemoryEntry, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let entry = conn
            .query_row(
                "SELECT id, kind, mem_key, body, trust, produced_sess, produced_seq,
                        sources_json, verdict_json, verdict_sess, verdict_seq,
                        salience, superseded_by, created_at, write_seq
                 FROM memory WHERE id = ?1",
                params![id.0.as_str()],
                row_to_entry,
            )
            .optional()?
            .ok_or_else(|| MemoryError::NotFound(id.clone()))?;
        Ok(entry.0)
    }

    fn query(&self, q: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        if q.limit == 0 {
            return Ok(Vec::new());
        }

        if matches!(self.options.recall, MemoryRecallMode::Semantic) && q.contains.is_some() {
            return self.query_semantic(q);
        }
        if matches!(self.options.recall, MemoryRecallMode::Hybrid) && q.contains.is_some() {
            return self.query_hybrid(q);
        }

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, mem_key, body, trust, produced_sess, produced_seq,
                    sources_json, verdict_json, verdict_sess, verdict_seq,
                    salience, superseded_by, created_at, write_seq
             FROM memory
             WHERE superseded_by IS NULL
             ORDER BY salience DESC, write_seq DESC",
        )?;
        let rows = stmt.query_map([], row_to_entry)?;

        let mut entries = Vec::new();
        for row in rows {
            let (entry, _) = row?;
            if !matches_query(&entry, q) {
                continue;
            }
            entries.push(entry);
            if entries.len() >= q.limit {
                break;
            }
        }
        Ok(entries)
    }

    fn delta_since(&self, cursor: MemoryCursor) -> Result<MemoryDelta, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, mem_key, body, trust, produced_sess, produced_seq,
                    sources_json, verdict_json, verdict_sess, verdict_seq,
                    salience, superseded_by, created_at, write_seq
             FROM memory
             WHERE write_seq > ?1
             ORDER BY write_seq ASC",
        )?;
        let rows = stmt.query_map(params![cursor.0], row_to_entry)?;

        let mut entries = Vec::new();
        let mut next_cursor = cursor.0;
        for row in rows {
            let (entry, write_seq) = row?;
            next_cursor = write_seq;
            entries.push(entry);
        }

        Ok(MemoryDelta {
            entries,
            cursor: MemoryCursor(next_cursor),
        })
    }

    fn write_policy(&self, draft: PolicyDraft) -> Result<PolicyId, MemoryError> {
        let id = policy_id(&draft.policy)?;
        let policy_json = serde_json::to_string(&draft.policy)?;
        let sources_json = serde_json::to_string(&draft.sources)?;
        let created_at = now_ms();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let seq = next_policy_write_seq(&tx)?;
        tx.execute(
            "INSERT INTO policy_records (
               id, policy_json, trust, produced_sess, produced_seq, sources_json,
               verdict_json, superseded_by, created_at, write_seq
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8)",
            params![
                id.0.as_str(),
                policy_json.as_str(),
                trust_to_str(Trust::Unverified),
                draft.produced_by.session.as_str(),
                draft.produced_by.seq,
                sources_json.as_str(),
                created_at,
                seq,
            ],
        )?;
        tx.commit()?;

        Ok(id)
    }

    fn get_policy(&self, id: &PolicyId) -> Result<PolicyRecord, MemoryError> {
        let conn = self.conn.lock().unwrap();
        let record = conn
            .query_row(
                "SELECT id, policy_json, trust, produced_sess, produced_seq,
                        sources_json, verdict_json, superseded_by, created_at
                 FROM policy_records WHERE id = ?1",
                params![id.0.as_str()],
                row_to_policy_record,
            )
            .optional()?
            .ok_or_else(|| MemoryError::NotFound(MemoryId(id.0.clone())))?;
        Ok(record)
    }
}

fn init_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(SCHEMA)?;
    let _ = conn.execute("ALTER TABLE memory ADD COLUMN verdict_sess TEXT", []);
    let _ = conn.execute("ALTER TABLE memory ADD COLUMN verdict_seq INTEGER", []);
    Ok(())
}

fn validate_options(options: &MemoryStoreOptions) -> Result<(), MemoryError> {
    match options.recall {
        MemoryRecallMode::Exact => Ok(()),
        MemoryRecallMode::Semantic => options
            .embedder
            .as_ref()
            .map(|_| ())
            .ok_or(MemoryError::MissingEmbedder),
        MemoryRecallMode::Hybrid => options
            .embedder
            .as_ref()
            .map(|_| ())
            .ok_or(MemoryError::MissingEmbedder),
    }
}

fn ensure_dims(embedder: &dyn Embedder, vector: &[f32]) -> Result<(), MemoryError> {
    if vector.len() == embedder.dims() {
        Ok(())
    } else {
        Err(MemoryError::Backend(anyhow::anyhow!(
            "embedder {} returned {} dims, expected {}",
            embedder.id(),
            vector.len(),
            embedder.dims()
        )))
    }
}

fn persist_embedding(
    conn: &Connection,
    id: &MemoryId,
    embedder: &dyn Embedder,
    vector: &[f32],
) -> Result<(), MemoryError> {
    let vector_json = serde_json::to_string(vector)?;
    conn.execute(
        "INSERT INTO memory_embeddings (memory_id, embedder_id, dims, vector_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(memory_id) DO UPDATE SET
           embedder_id = excluded.embedder_id,
           dims = excluded.dims,
           vector_json = excluded.vector_json,
           created_at = excluded.created_at",
        params![
            id.0.as_str(),
            embedder.id(),
            embedder.dims() as i64,
            vector_json.as_str(),
            now_ms(),
        ],
    )?;
    Ok(())
}

fn load_embedding(
    conn: &Connection,
    id: &MemoryId,
    embedder: &dyn Embedder,
) -> Result<Option<(String, usize, Vec<f32>)>, MemoryError> {
    let row = conn
        .query_row(
            "SELECT embedder_id, dims, vector_json
             FROM memory_embeddings
             WHERE memory_id = ?1",
            params![id.0.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((embedder_id, dims, vector_json)) = row else {
        return Ok(None);
    };
    if embedder_id != embedder.id() || dims as usize != embedder.dims() {
        return Ok(Some((embedder_id, dims as usize, Vec::new())));
    }
    let vector = serde_json::from_str::<Vec<f32>>(&vector_json)?;
    Ok(Some((embedder_id, dims as usize, vector)))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut a_norm = 0.0;
    let mut b_norm = 0.0;
    for (left, right) in a.iter().zip(b) {
        dot += left * right;
        a_norm += left * left;
        b_norm += right * right;
    }
    if a_norm == 0.0 || b_norm == 0.0 {
        0.0
    } else {
        dot / (a_norm.sqrt() * b_norm.sqrt())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn memory_id(kind: &EntryKind, key: Option<&str>, body: &str) -> MemoryId {
    let mut h = Sha256::new();
    h.update(kind_to_str(*kind).as_bytes());
    h.update(key.unwrap_or_default().as_bytes());
    h.update(body.as_bytes());
    MemoryId(format!("{:x}", h.finalize()))
}

fn policy_id(policy: &super::types::Policy) -> Result<PolicyId, MemoryError> {
    let mut h = Sha256::new();
    h.update(policy.schema_version.to_le_bytes());
    h.update(serde_json::to_vec(policy)?);
    Ok(PolicyId(format!("{:x}", h.finalize())))
}

fn next_write_seq(tx: &Transaction<'_>) -> Result<i64, rusqlite::Error> {
    let max: Option<i64> = tx
        .query_row("SELECT MAX(write_seq) FROM memory", [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(max.unwrap_or(0) + 1)
}

fn next_policy_write_seq(tx: &Transaction<'_>) -> Result<i64, rusqlite::Error> {
    let max: Option<i64> = tx
        .query_row("SELECT MAX(write_seq) FROM policy_records", [], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    Ok(max.unwrap_or(0) + 1)
}

fn should_supersede(kind: EntryKind, key: Option<&str>) -> bool {
    matches!(
        kind,
        EntryKind::Fact | EntryKind::Lesson | EntryKind::Playbook
    ) && key.is_some()
}

fn active_entry_for_key(
    tx: &Transaction<'_>,
    kind: EntryKind,
    key: Option<&str>,
) -> Result<Option<MemoryId>, rusqlite::Error> {
    let Some(key) = key else {
        return Ok(None);
    };
    tx.query_row(
        "SELECT id FROM memory
         WHERE kind = ?1 AND mem_key = ?2 AND superseded_by IS NULL
         ORDER BY write_seq DESC
         LIMIT 1",
        params![kind_to_str(kind), key],
        |r| Ok(MemoryId(r.get(0)?)),
    )
    .optional()
}

fn update_trust(
    conn: &Arc<Mutex<Connection>>,
    id: &MemoryId,
    trust: Trust,
    verdict_json: Option<String>,
    verdict_ref: Option<LedgerRef>,
    superseded_by: Option<MemoryId>,
    reconcile: Reconcile,
) -> Result<(), MemoryError> {
    let mut conn = conn.lock().unwrap();
    let tx = conn.transaction()?;
    let exists: Option<String> = tx
        .query_row(
            "SELECT id FROM memory WHERE id = ?1",
            params![id.0.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(MemoryError::NotFound(id.clone()));
    }

    let seq = next_write_seq(&tx)?;
    let (verdict_sess, verdict_seq) = verdict_ref
        .map(|ledger| (Some(ledger.session), Some(ledger.seq)))
        .unwrap_or((None, None));
    let rows = match (verdict_json, superseded_by) {
        (Some(verdict_json), Some(superseded_by)) => tx.execute(
            "UPDATE memory
             SET trust = ?2, verdict_json = ?3, verdict_sess = ?4, verdict_seq = ?5,
                 superseded_by = ?6, write_seq = ?7
             WHERE id = ?1",
            params![
                id.0.as_str(),
                trust_to_str(trust),
                verdict_json,
                verdict_sess,
                verdict_seq,
                superseded_by.0,
                seq
            ],
        )?,
        (Some(verdict_json), None) => tx.execute(
            "UPDATE memory
             SET trust = ?2, verdict_json = ?3, verdict_sess = ?4, verdict_seq = ?5,
                 write_seq = ?6
             WHERE id = ?1",
            params![
                id.0.as_str(),
                trust_to_str(trust),
                verdict_json,
                verdict_sess,
                verdict_seq,
                seq
            ],
        )?,
        (None, Some(superseded_by)) => tx.execute(
            "UPDATE memory SET trust = ?2, superseded_by = ?3, write_seq = ?4 WHERE id = ?1",
            params![id.0.as_str(), trust_to_str(trust), superseded_by.0, seq],
        )?,
        (None, None) => tx.execute(
            "UPDATE memory SET trust = ?2, write_seq = ?3 WHERE id = ?1",
            params![id.0.as_str(), trust_to_str(trust), seq],
        )?,
    };
    match reconcile {
        Reconcile::Promoted => {
            let prior_seq = next_write_seq(&tx)?;
            tx.execute(
                "UPDATE memory
                 SET trust = ?2, write_seq = ?3
                 WHERE superseded_by = ?1 AND trust = ?4",
                params![
                    id.0.as_str(),
                    trust_to_str(Trust::Stale),
                    prior_seq,
                    trust_to_str(Trust::Verified),
                ],
            )?;
        }
        Reconcile::Rejected => {
            let prior_seq = next_write_seq(&tx)?;
            tx.execute(
                "UPDATE memory
                 SET superseded_by = NULL, write_seq = ?2
                 WHERE superseded_by = ?1 AND trust = ?3",
                params![id.0.as_str(), prior_seq, trust_to_str(Trust::Verified),],
            )?;
        }
        Reconcile::None => {}
    }
    tx.commit()?;

    if rows == 0 {
        Err(MemoryError::NotFound(id.clone()))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Reconcile {
    None,
    Promoted,
    Rejected,
}

fn matches_query(entry: &MemoryEntry, q: &MemoryQuery) -> bool {
    if !matches_query_filters(entry, q) {
        return false;
    }
    if q.contains
        .as_ref()
        .is_some_and(|needle| !entry.body.contains(needle))
    {
        return false;
    }
    true
}

fn matches_query_filters(entry: &MemoryEntry, q: &MemoryQuery) -> bool {
    if entry.trust == Trust::Rejected || entry.trust.rank() < q.min_trust.rank() {
        return false;
    }
    if q.kind.is_some_and(|kind| entry.kind != kind) {
        return false;
    }
    if q.key
        .as_ref()
        .is_some_and(|key| entry.key.as_ref() != Some(key))
    {
        return false;
    }
    true
}

fn row_to_entry(row: &Row<'_>) -> Result<(MemoryEntry, i64), rusqlite::Error> {
    let id: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let key: Option<String> = row.get(2)?;
    let body: String = row.get(3)?;
    let trust: String = row.get(4)?;
    let produced_sess: String = row.get(5)?;
    let produced_seq: i64 = row.get(6)?;
    let sources_json: String = row.get(7)?;
    let verdict_json: Option<String> = row.get(8)?;
    let verdict_sess: Option<String> = row.get(9)?;
    let verdict_seq: Option<i64> = row.get(10)?;
    let salience: f32 = row.get(11)?;
    let superseded_by: Option<String> = row.get(12)?;
    let created_ms: i64 = row.get(13)?;
    let write_seq: i64 = row.get(14)?;

    let kind = str_to_kind(&kind).map_err(from_sql_error)?;
    let trust = str_to_trust(&trust).map_err(from_sql_error)?;
    let sources = serde_json::from_str::<Vec<LedgerRef>>(&sources_json).unwrap_or_default();
    let verified_by = match (trust, verdict_json) {
        (Trust::Verified, Some(json)) => serde_json::from_str(&json).ok(),
        _ => None,
    };
    let verified_by_ref = match (verified_by.as_ref(), verdict_sess, verdict_seq) {
        (Some(_), Some(session), Some(seq)) => Some(LedgerRef { session, seq }),
        _ => None,
    };

    Ok((
        MemoryEntry {
            id: MemoryId(id),
            kind,
            key,
            body,
            trust,
            provenance: Provenance {
                produced_by: LedgerRef {
                    session: produced_sess,
                    seq: produced_seq,
                },
                verified_by,
                verified_by_ref,
                sources,
            },
            salience,
            created_ms,
            superseded_by: superseded_by.map(MemoryId),
        },
        write_seq,
    ))
}

fn row_to_policy_record(row: &Row<'_>) -> Result<PolicyRecord, rusqlite::Error> {
    let id: String = row.get(0)?;
    let policy_json: String = row.get(1)?;
    let trust: String = row.get(2)?;
    let produced_sess: String = row.get(3)?;
    let produced_seq: i64 = row.get(4)?;
    let sources_json: String = row.get(5)?;
    let verdict_json: Option<String> = row.get(6)?;
    let superseded_by: Option<String> = row.get(7)?;
    let created_ms: i64 = row.get(8)?;

    let policy = serde_json::from_str::<super::types::Policy>(&policy_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let trust = str_to_trust(&trust).map_err(from_sql_error)?;
    let sources = serde_json::from_str::<Vec<LedgerRef>>(&sources_json).unwrap_or_default();
    let verified_by = match (trust, verdict_json) {
        (Trust::Verified, Some(json)) => serde_json::from_str(&json).ok(),
        _ => None,
    };

    Ok(PolicyRecord {
        id: PolicyId(id),
        policy,
        trust,
        provenance: Provenance {
            produced_by: LedgerRef {
                session: produced_sess,
                seq: produced_seq,
            },
            verified_by,
            verified_by_ref: None,
            sources,
        },
        created_ms,
        superseded_by: superseded_by.map(PolicyId),
    })
}

fn from_sql_error(msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, msg)),
    )
}

fn kind_to_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Goal => "goal",
        EntryKind::Decision => "decision",
        EntryKind::Fact => "fact",
        EntryKind::Lesson => "lesson",
        EntryKind::Playbook => "playbook",
        EntryKind::Artifact => "artifact",
        EntryKind::Scratch => "scratch",
        EntryKind::ContextSummary => "context_summary",
    }
}

fn str_to_kind(kind: &str) -> Result<EntryKind, String> {
    match kind {
        "goal" => Ok(EntryKind::Goal),
        "decision" => Ok(EntryKind::Decision),
        "fact" => Ok(EntryKind::Fact),
        "lesson" => Ok(EntryKind::Lesson),
        "playbook" => Ok(EntryKind::Playbook),
        "artifact" => Ok(EntryKind::Artifact),
        "scratch" => Ok(EntryKind::Scratch),
        "context_summary" => Ok(EntryKind::ContextSummary),
        other => Err(format!("unknown memory kind: {other}")),
    }
}

fn trust_to_str(trust: Trust) -> &'static str {
    match trust {
        Trust::Unverified => "unverified",
        Trust::Verified => "verified",
        Trust::Rejected => "rejected",
        Trust::Stale => "stale",
    }
}

fn str_to_trust(trust: &str) -> Result<Trust, String> {
    match trust {
        "unverified" => Ok(Trust::Unverified),
        "verified" => Ok(Trust::Verified),
        "rejected" => Ok(Trust::Rejected),
        "stale" => Ok(Trust::Stale),
        other => Err(format!("unknown memory trust tier: {other}")),
    }
}
