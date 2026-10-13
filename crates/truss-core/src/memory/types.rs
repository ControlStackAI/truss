//! Core memory data model — entries, trust tiers, provenance, queries.
//!
//! Pure data: no I/O, no vendor deps. Timestamps are passed in as epoch-ms
//! (`i64`) so this layer stays deterministic; the SQLite store stamps `now`
//! the same way [`crate::store`] does.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delegation::Verdict;

/// Content-addressed entry id (hex of sha256 over `kind || key || body`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

/// Pointer into the signed journal ([`crate::store`]) — `(session, seq)` is the
/// journal primary key, so this is a stable, verifiable reference to the event
/// that produced or verified an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerRef {
    pub session: String,
    pub seq: i64,
}

/// Inclusive journal range. The journal remains authoritative, so compacted
/// context summaries point back to the exact span that can be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerRange {
    pub start: LedgerRef,
    pub end: LedgerRef,
}

/// Typed memory — not a flat scratchpad. Each kind has its own mutability and
/// compaction policy (see the architecture doc §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// North-star DoD / intent for a contract. Append+supersede; **never compacted**.
    Goal,
    /// A choice made + rationale. Append-only, immutable log.
    Decision,
    /// A durable learned fact. Supersede-by-`key`; salience-scored.
    Fact,
    /// A correction / confirmed approach + why. Supersede-by-`key`; salience-scored.
    Lesson,
    /// A governed procedure/skill. Supersede-by-`key`; verified before trusted recall.
    Playbook,
    /// A verified output + where it lives. Append; compacted by reference.
    Artifact,
    /// Ephemeral working note. Aggressively compacted.
    Scratch,
    /// Deterministic summary of a journal span elided from the projected context.
    ContextSummary,
}

/// Trust tier. A write is never trusted on arrival — it is promoted only by a
/// passing [`Verdict`]. The tier is *always* surfaced to consumers; no mode
/// relabels unverified content as trusted (see [`crate::memory::policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Just written by the model. Visible in-run as flagged scratch; not surfaced
    /// to future runs as trusted.
    Unverified,
    /// Promoted after an N-reviewer pass returned PASS. Carries the signed verdict.
    Verified,
    /// Refuted by the panel. Retained for audit, never surfaced.
    Rejected,
    /// Was verified, later contradicted by a newer verified entry or a failed
    /// re-check. Demoted, not deleted.
    Stale,
}

impl Trust {
    /// Ordering for `min_trust` query filters (Unverified < Stale < Verified;
    /// Rejected is out-of-band and never matched by a `min_trust` floor).
    pub fn rank(self) -> u8 {
        match self {
            Trust::Rejected => 0,
            Trust::Unverified => 1,
            Trust::Stale => 2,
            Trust::Verified => 3,
        }
    }
}

/// The evidence chain behind an entry. This is what makes memory auditable
/// rather than vibes: "why do you believe X?" resolves to ledger events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// The ledger event (tool call / write) that produced this entry.
    pub produced_by: LedgerRef,
    /// The signed N-reviewer verdict that promoted it (present iff `Verified`).
    pub verified_by: Option<Verdict>,
    /// The signed journal event that recorded the promotion verdict, when the
    /// promotion ran with a durable ledger.
    #[serde(default)]
    pub verified_by_ref: Option<LedgerRef>,
    /// Supporting evidence the model cited at write time.
    pub sources: Vec<LedgerRef>,
}

/// A stored memory entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub kind: EntryKind,
    /// Supersede key for `Fact`/`Lesson` (e.g. a stable slug). `None` for
    /// append-only kinds.
    pub key: Option<String>,
    /// Model-authored content.
    pub body: String,
    pub trust: Trust,
    pub provenance: Provenance,
    /// Relevance score for ranking + eviction (0.0–1.0). Set by salience pass.
    pub salience: f32,
    pub created_ms: i64,
    /// If superseded, points at the entry that replaced this one.
    pub superseded_by: Option<MemoryId>,
}

/// A pending write from the model (before it lands `Unverified`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDraft {
    pub kind: EntryKind,
    pub key: Option<String>,
    pub body: String,
    pub produced_by: LedgerRef,
    pub sources: Vec<LedgerRef>,
}

/// Content-addressed policy record id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PolicyId(pub String);

/// The bounded set of self-tunable policy surfaces. L0 core and operator-signed
/// boundaries are intentionally absent: they are not policy targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyTarget {
    Routing,
    PersonaSelf,
    ToolSet,
    Verification,
    Salience,
    GoalRepin,
    Other(String),
}

/// Typed, versioned policy object. This is the governed write target L3
/// optimizers will eventually propose against; this milestone only stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub schema_version: u32,
    pub target: PolicyTarget,
    pub key: String,
    pub body: Value,
}

/// A stored governed policy record. It mirrors memory's trust/provenance shape
/// so future policy promotion can reuse the same panel and ledger machinery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRecord {
    pub id: PolicyId,
    pub policy: Policy,
    pub trust: Trust,
    pub provenance: Provenance,
    pub created_ms: i64,
    pub superseded_by: Option<PolicyId>,
}

/// A pending governed-policy write before it lands `Unverified`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDraft {
    pub policy: Policy,
    pub produced_by: LedgerRef,
    pub sources: Vec<LedgerRef>,
}

/// Query against the store. Defaults to "verified only" so the model's `view`
/// pulls trusted memory by default (pull-not-push of L2).
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub kind: Option<EntryKind>,
    pub key: Option<String>,
    /// Trust floor. Defaults to `Verified`.
    pub min_trust: Trust,
    /// Text/semantic match (M5: backed by a vector index).
    pub contains: Option<String>,
    pub limit: usize,
}

impl Default for MemoryQuery {
    fn default() -> Self {
        MemoryQuery {
            kind: None,
            key: None,
            min_trust: Trust::Verified,
            contains: None,
            limit: 32,
        }
    }
}

/// Cursor for delta rehydration — the highest journal `seq` the model has
/// already seen reflected in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCursor(pub i64);

/// What changed in memory since a cursor — injected on resume/re-pin instead of
/// the whole store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDelta {
    pub entries: Vec<MemoryEntry>,
    pub cursor: MemoryCursor,
}
