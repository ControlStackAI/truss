//! Typed, trust-tiered, provenance-tracked memory + layered identity (L0–L4).
//!
//! DRAFT skeleton (2026-06-16) for the Memory & Identity Architecture —
//! see `docs/memory-identity-architecture.md`. **Not yet wired into `lib.rs`**:
//! these types are the M1 starting point and are deliberately kept out of the
//! compiled build until the milestone begins, so the green test suite is
//! untouched. Add `pub mod memory;` to `lib.rs` to activate.
//!
//! Design in one breath: the model curates memory through the native
//! `memory_20250818` tool; Truss governs *trust, lifetime, eviction, and
//! injection*. A write lands `Unverified` and only becomes `Verified` when the
//! existing N-reviewer panel ([`crate::delegation::Verdict`]) signs off — the
//! same machinery that gates contract completion, now gating *belief*. Every
//! entry links to the signed ledger ([`crate::store`]) for provenance.
//!
//! Naming note: `crate::identity` is the **ed25519 signing key**, not agent
//! persona. The L0 agent-identity concept lives here as [`persona::Persona`] to
//! avoid the collision.
//!
//! Module map:
//! - [`types`]   — `MemoryEntry`, `EntryKind`, `Trust`, `Provenance`, queries/deltas.
//! - [`policy`]  — `VerificationPolicy` + `IdentityPolicy` (user-selectable knobs).
//! - [`store`]   — the substrate-agnostic [`store::MemoryStore`] trait (M1: SQLite).
//! - [`persona`] — L0 identity, split into operator-signed `core` + self-amendable `self`.

pub mod ai_memory;
pub mod context;
pub mod embed;
pub mod persona;
pub mod policy;
pub mod store;
pub mod types;

pub use ai_memory::AiMemoryStore;
pub use context::{
    context_pressure_window, recall_verified_lessons, recall_verified_playbooks, AssembledContext,
    ContextAssembler, ContextTier, GoalLedger, LayeredContextConfig, LessonRecall, PlaybookRecall,
};
pub use embed::{DeterministicEmbedder, Embedder};
pub use persona::{
    Amendment, AmendmentDecision, AmendmentTarget, IdentityJournalEvent, Persona, PersonaCore,
    PersonaError, PersonaSelf,
};
pub use policy::{
    CompactionMode, CompactionPolicy, CompactionPolicyLayer, ConflictResolution,
    EffectiveBudgetPolicy, EffectiveBudgetPolicyLayer, IdentityPolicy, LessonExtractionPolicy,
    MemoryConfig, MemoryConfigLayer, MemoryEmbeddingBackend, MemoryRecallMode, MemoryScope,
    MemoryStoreBackend, ProjectionConfig, SaliencePolicy, VerificationPolicy,
};
pub use store::{
    MemoryEmbeddingMetadata, MemoryError, MemoryStore, MemoryStoreOptions, ReindexSummary,
    SqliteMemoryStore,
};
pub use types::{
    EntryKind, LedgerRange, LedgerRef, MemoryCursor, MemoryDelta, MemoryDraft, MemoryEntry,
    MemoryId, MemoryQuery, Policy, PolicyDraft, PolicyId, PolicyRecord, PolicyTarget, Provenance,
    Trust,
};
