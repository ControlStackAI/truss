//! User-selectable policy knobs for memory verification and identity amendment.
//!
//! Both are *settings*, layered global → per-agent → per-contract. Resolution
//! happens at session build; the loop reads the resolved [`MemoryConfig`].
//! Design rationale + the "what do we lose by making these selectable" analysis
//! live in `docs/memory-identity-architecture.md` §3.5.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::context::ContextTier;

/// Durable substrate used for governed memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStoreBackend {
    /// Keep memory in Truss's journal-adjacent SQLite database.
    #[default]
    Sqlite,
    /// Use the authenticated ai-memory Unix-domain broker.
    AiMemory,
}

/// Governed context compaction action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    /// Default-safe: observe pressure only; never alter the projected history.
    #[default]
    Off,
    /// Replace the elided middle with a deterministic summary message and store
    /// that summary as governed memory.
    Summarize,
    /// Store a summary for audit/recall but omit it from the active context.
    Evict,
}

/// Resolved governed-compaction policy. Defaults are safe: the mode is off, so
/// existing runs keep identical request shapes until a caller opts in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionPolicy {
    #[serde(default)]
    pub mode: CompactionMode,
    #[serde(default = "default_compaction_threshold")]
    pub threshold: f64,
    #[serde(default = "default_compaction_min_tail")]
    pub min_tail: usize,
    /// Opt in to CC-3 compact-ahead behavior. Default false preserves CC-2
    /// request shapes and event streams unless explicitly enabled.
    #[serde(default)]
    pub anticipate: bool,
    #[serde(default = "default_compaction_lookahead_steps")]
    pub lookahead_steps: u64,
    #[serde(default = "default_compaction_exempt_tiers")]
    pub exempt_tiers: Vec<ContextTier>,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            mode: CompactionMode::Off,
            threshold: default_compaction_threshold(),
            min_tail: default_compaction_min_tail(),
            anticipate: false,
            lookahead_steps: default_compaction_lookahead_steps(),
            exempt_tiers: default_compaction_exempt_tiers(),
        }
    }
}

impl CompactionPolicy {
    pub fn enabled(&self) -> bool {
        !matches!(self.mode, CompactionMode::Off)
    }
}

/// A partial config layer for governed compaction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompactionPolicyLayer {
    #[serde(default)]
    pub mode: Option<CompactionMode>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub min_tail: Option<usize>,
    #[serde(default)]
    pub anticipate: Option<bool>,
    #[serde(default)]
    pub lookahead_steps: Option<u64>,
    #[serde(default)]
    pub exempt_tiers: Option<Vec<ContextTier>>,
}

/// Resolved EC-1 effective-context budget policy.
///
/// `ratio` is the conservative default path: unknown models get a usable budget
/// at or below the provider's nominal context window. `overrides` is an exact
/// model-id table for measured budgets where the ratio is too blunt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveBudgetPolicy {
    #[serde(default = "default_effective_budget_ratio")]
    pub ratio: f64,
    #[serde(default)]
    pub overrides: BTreeMap<String, usize>,
}

impl Default for EffectiveBudgetPolicy {
    fn default() -> Self {
        Self {
            ratio: default_effective_budget_ratio(),
            overrides: BTreeMap::new(),
        }
    }
}

impl EffectiveBudgetPolicy {
    pub fn resolve(&self, model_id: Option<&str>, nominal_window: usize) -> usize {
        if let Some(budget) = model_id.and_then(|model| self.overrides.get(model)) {
            return (*budget).max(1);
        }

        let nominal_window = nominal_window.max(1);
        (((nominal_window as f64) * clamp_effective_budget_ratio(self.ratio)).ceil() as usize)
            .clamp(1, nominal_window)
    }
}

/// A partial config layer for EC-1 effective context budgets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectiveBudgetPolicyLayer {
    #[serde(default)]
    pub ratio: Option<f64>,
    #[serde(default)]
    pub overrides: Option<BTreeMap<String, usize>>,
}

pub fn default_effective_budget_ratio() -> f64 {
    0.60
}

fn clamp_effective_budget_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        default_effective_budget_ratio()
    }
}

fn default_compaction_threshold() -> f64 {
    0.70
}

fn default_compaction_min_tail() -> usize {
    6
}

fn default_compaction_lookahead_steps() -> u64 {
    1
}

/// When (and whether) `Unverified → Verified` promotion runs.
///
/// Invariant that makes every mode safe to expose: **`Verified` always means a
/// panel signed off.** This knob changes *when* verification runs and *what*
/// gets verified — never what the trust label asserts. `Off` does not relabel
/// junk as trusted; it leaves entries `Unverified` and honestly labeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPolicy {
    /// No promotion. Writes stay `Unverified`, usable in-run, never surfaced to
    /// future runs as trusted. Cheapest; no trust guarantee.
    Off,
    /// Verify on first **recall**, not on write — pay only for memory actually
    /// re-used. The default: amortized cost, honest trust.
    #[default]
    Lazy,
    /// Sync N-reviewer pass before promotion, on write. Strongest freshness,
    /// highest cost. For high-stakes / shared memory.
    Eager,
    /// Cheap heuristic gate always; full panel only for `Fact`/`Decision` or
    /// writes flagged high-impact. Balanced.
    Tiered,
}

/// Who may amend the L0 `self` layer (voice/persona). The `core` layer
/// (boundaries/safety/mandate) is operator-signed and **never** self-amendable
/// regardless of this setting — see [`super::persona`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdentityPolicy {
    /// Operator-signed only. The agent may *propose* but not commit.
    Locked,
    /// Agent drafts an amendment → verdict gate / operator approval → commit.
    /// The default.
    #[default]
    Propose,
    /// Agent commits directly (subject to the four structural guards in
    /// [`super::persona`]: core locked, run-boundary commit, signed deliberate
    /// tool call, reversible ledger event).
    SelfAmend,
}

/// L2 recall boundary. Defaults to per-agent so two agents do not implicitly
/// share beliefs unless the operator opts into wider scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryScope {
    #[default]
    PerAgent,
    PerProject,
    Shared,
}

/// How contradictory verified entries are reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictResolution {
    /// A verified entry beats an unverified/stale/rejected one; verified-vs-
    /// verified stays in the panel/verdict lane.
    #[default]
    VerifiedWins,
    /// The highest write sequence wins, regardless of older verification.
    LatestWins,
    /// Escalate to an explicit panel before either fact is preferred.
    PanelArbitrated,
    /// Keep both entries visible with contradiction metadata.
    KeepBothFlagged,
}

/// M5 seam. The field is configurable now so experiments can select the policy,
/// but eviction/scoring is intentionally not implemented until M5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SaliencePolicy {
    #[default]
    Off,
    HarnessRecency,
    ModelScored,
    Hybrid,
}

/// Query-time recall backend. Defaults to exact substring/lexical behavior so
/// existing runs keep identical memory semantics until explicitly opted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryRecallMode {
    #[default]
    Exact,
    Semantic,
    Hybrid,
}

/// Embedding backend selected when semantic recall is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryEmbeddingBackend {
    /// Hermetic feature-hashing embedder. Intended for tests and local smoke
    /// runs; deterministic but not a quality semantic model.
    Deterministic,
    /// OpenAI-compatible `/v1/embeddings` backend, configured by env/config.
    #[default]
    OpenAi,
}

/// Whether contract-close lesson extraction runs after a signed verdict.
///
/// `Off` is the default-safe mode and must be zero-cost: no provider call, no
/// memory write. `On` always runs the extractor. `ModelScored` asks the model to
/// attach salience and writes only lessons above
/// [`MemoryConfig::extraction_min_salience`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LessonExtractionPolicy {
    #[default]
    Off,
    On,
    ModelScored,
}

/// Context projection knobs. `keep_tail = 0` preserves the pre-M4 default of no
/// projection unless the operator opts in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionConfig {
    #[serde(default)]
    pub keep_tail: usize,
    #[serde(default = "default_compaction_exempt_tiers")]
    pub compaction_exempt_tiers: Vec<ContextTier>,
}

impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            keep_tail: 0,
            compaction_exempt_tiers: default_compaction_exempt_tiers(),
        }
    }
}

fn default_compaction_exempt_tiers() -> Vec<ContextTier> {
    vec![
        ContextTier::L0Core,
        ContextTier::L0Self,
        ContextTier::L1Principal,
        ContextTier::L3Goal,
    ]
}

fn default_goal_repin_every() -> usize {
    8
}

fn default_extraction_min_salience() -> f32 {
    0.70
}

fn default_lesson_recall_max_lessons() -> usize {
    5
}

fn default_lesson_recall_token_budget() -> usize {
    512
}

fn default_playbook_recall_max_playbooks() -> usize {
    0
}

fn default_playbook_recall_token_budget() -> usize {
    512
}

fn default_embedding_model() -> String {
    "text-embedding-3-small".into()
}

fn default_embedding_api_key_env() -> String {
    "OPENAI_API_KEY".into()
}

fn default_embedding_dims() -> usize {
    1536
}

/// A single config layer. `None` means "inherit from the lower-precedence
/// layer"; resolution happens global → per-agent → per-contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryConfigLayer {
    #[serde(default)]
    pub store_backend: Option<MemoryStoreBackend>,
    #[serde(default)]
    pub ai_memory_socket: Option<PathBuf>,
    #[serde(default)]
    pub recall: Option<MemoryRecallMode>,
    #[serde(default)]
    pub embedding_backend: Option<MemoryEmbeddingBackend>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub embedding_base_url: Option<String>,
    #[serde(default)]
    pub embedding_api_key_env: Option<String>,
    #[serde(default)]
    pub embedding_dims: Option<usize>,
    #[serde(default)]
    pub verification: Option<VerificationPolicy>,
    #[serde(default)]
    pub identity: Option<IdentityPolicy>,
    #[serde(default)]
    pub scope: Option<MemoryScope>,
    #[serde(default)]
    pub conflict: Option<ConflictResolution>,
    #[serde(default)]
    pub goal_repin_every: Option<usize>,
    #[serde(default)]
    pub projection: Option<ProjectionConfig>,
    #[serde(default)]
    pub compaction: Option<CompactionPolicyLayer>,
    #[serde(default)]
    pub effective_budget: Option<EffectiveBudgetPolicyLayer>,
    #[serde(default)]
    pub salience: Option<SaliencePolicy>,
    #[serde(default)]
    pub extraction: Option<LessonExtractionPolicy>,
    #[serde(default)]
    pub extraction_min_salience: Option<f32>,
    #[serde(default)]
    pub lesson_recall_max_lessons: Option<usize>,
    #[serde(default)]
    pub lesson_recall_token_budget: Option<usize>,
    #[serde(default)]
    pub playbook_recall_max_playbooks: Option<usize>,
    #[serde(default)]
    pub playbook_recall_token_budget: Option<usize>,
}

/// Resolved memory/identity configuration for a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryConfig {
    #[serde(default)]
    pub store_backend: MemoryStoreBackend,
    #[serde(default)]
    pub ai_memory_socket: Option<PathBuf>,
    pub recall: MemoryRecallMode,
    pub embedding_backend: MemoryEmbeddingBackend,
    pub embedding_model: String,
    pub embedding_base_url: Option<String>,
    pub embedding_api_key_env: String,
    pub embedding_dims: usize,
    pub verification: VerificationPolicy,
    pub identity: IdentityPolicy,
    pub scope: MemoryScope,
    pub conflict: ConflictResolution,
    pub goal_repin_every: usize,
    pub projection: ProjectionConfig,
    pub compaction: CompactionPolicy,
    pub effective_budget: EffectiveBudgetPolicy,
    pub salience: SaliencePolicy,
    pub extraction: LessonExtractionPolicy,
    pub extraction_min_salience: f32,
    pub lesson_recall_max_lessons: usize,
    pub lesson_recall_token_budget: usize,
    pub playbook_recall_max_playbooks: usize,
    pub playbook_recall_token_budget: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            store_backend: MemoryStoreBackend::default(),
            ai_memory_socket: None,
            recall: MemoryRecallMode::default(),
            embedding_backend: MemoryEmbeddingBackend::default(),
            embedding_model: default_embedding_model(),
            embedding_base_url: None,
            embedding_api_key_env: default_embedding_api_key_env(),
            embedding_dims: default_embedding_dims(),
            verification: VerificationPolicy::default(),
            identity: IdentityPolicy::default(),
            scope: MemoryScope::default(),
            conflict: ConflictResolution::default(),
            goal_repin_every: default_goal_repin_every(),
            projection: ProjectionConfig::default(),
            compaction: CompactionPolicy::default(),
            effective_budget: EffectiveBudgetPolicy::default(),
            salience: SaliencePolicy::default(),
            extraction: LessonExtractionPolicy::default(),
            extraction_min_salience: default_extraction_min_salience(),
            lesson_recall_max_lessons: default_lesson_recall_max_lessons(),
            lesson_recall_token_budget: default_lesson_recall_token_budget(),
            playbook_recall_max_playbooks: default_playbook_recall_max_playbooks(),
            playbook_recall_token_budget: default_playbook_recall_token_budget(),
        }
    }
}

impl MemoryConfig {
    /// Resolve layered policy at session build. Later layers override only the
    /// fields they explicitly set.
    pub fn resolve(
        global: Option<&MemoryConfigLayer>,
        agent: Option<&MemoryConfigLayer>,
        contract: Option<&MemoryConfigLayer>,
    ) -> Self {
        let mut cfg = MemoryConfig::default();
        for layer in [global, agent, contract].into_iter().flatten() {
            if let Some(backend) = layer.store_backend {
                cfg.store_backend = backend;
            }
            if let Some(socket) = &layer.ai_memory_socket {
                cfg.ai_memory_socket = Some(socket.clone());
            }
            if let Some(recall) = layer.recall {
                cfg.recall = recall;
            }
            if let Some(backend) = layer.embedding_backend {
                cfg.embedding_backend = backend;
            }
            if let Some(model) = &layer.embedding_model {
                cfg.embedding_model = model.clone();
            }
            if let Some(base_url) = &layer.embedding_base_url {
                cfg.embedding_base_url = Some(base_url.clone());
            }
            if let Some(api_key_env) = &layer.embedding_api_key_env {
                cfg.embedding_api_key_env = api_key_env.clone();
            }
            if let Some(dims) = layer.embedding_dims {
                cfg.embedding_dims = dims.max(1);
            }
            if let Some(verification) = layer.verification {
                cfg.verification = verification;
            }
            if let Some(identity) = layer.identity {
                cfg.identity = identity;
            }
            if let Some(scope) = layer.scope {
                cfg.scope = scope;
            }
            if let Some(conflict) = layer.conflict {
                cfg.conflict = conflict;
            }
            if let Some(goal_repin_every) = layer.goal_repin_every {
                cfg.goal_repin_every = goal_repin_every.max(1);
            }
            if let Some(projection) = &layer.projection {
                cfg.projection = projection.clone();
            }
            if let Some(compaction) = &layer.compaction {
                if let Some(mode) = compaction.mode {
                    cfg.compaction.mode = mode;
                }
                if let Some(threshold) = compaction.threshold {
                    cfg.compaction.threshold = threshold.clamp(0.0, 1.0);
                }
                if let Some(min_tail) = compaction.min_tail {
                    cfg.compaction.min_tail = min_tail;
                }
                if let Some(anticipate) = compaction.anticipate {
                    cfg.compaction.anticipate = anticipate;
                }
                if let Some(lookahead_steps) = compaction.lookahead_steps {
                    cfg.compaction.lookahead_steps = lookahead_steps;
                }
                if let Some(exempt_tiers) = &compaction.exempt_tiers {
                    cfg.compaction.exempt_tiers = exempt_tiers.clone();
                }
            }
            if let Some(effective_budget) = &layer.effective_budget {
                if let Some(ratio) = effective_budget.ratio {
                    cfg.effective_budget.ratio = clamp_effective_budget_ratio(ratio);
                }
                if let Some(overrides) = &effective_budget.overrides {
                    cfg.effective_budget.overrides.extend(overrides.clone());
                }
            }
            if let Some(salience) = layer.salience {
                cfg.salience = salience;
            }
            if let Some(extraction) = layer.extraction {
                cfg.extraction = extraction;
            }
            if let Some(threshold) = layer.extraction_min_salience {
                cfg.extraction_min_salience = threshold.clamp(0.0, 1.0);
            }
            if let Some(max_lessons) = layer.lesson_recall_max_lessons {
                cfg.lesson_recall_max_lessons = max_lessons;
            }
            if let Some(token_budget) = layer.lesson_recall_token_budget {
                cfg.lesson_recall_token_budget = token_budget;
            }
            if let Some(max_playbooks) = layer.playbook_recall_max_playbooks {
                cfg.playbook_recall_max_playbooks = max_playbooks;
            }
            if let Some(token_budget) = layer.playbook_recall_token_budget {
                cfg.playbook_recall_token_budget = token_budget;
            }
        }
        cfg
    }

    /// Whether a draft of this kind should be verified eagerly (on write) under
    /// the current policy. `Lazy`/`Off` always return false here; `Tiered`
    /// returns true only for high-stakes kinds.
    pub fn verify_on_write(&self, kind: super::types::EntryKind) -> bool {
        use super::types::EntryKind::*;
        match self.verification {
            VerificationPolicy::Off | VerificationPolicy::Lazy => false,
            VerificationPolicy::Eager => true,
            VerificationPolicy::Tiered => matches!(kind, Fact | Decision),
        }
    }

    /// Whether an identity amendment may commit without operator approval.
    pub fn self_amend_allowed(&self) -> bool {
        matches!(self.identity, IdentityPolicy::SelfAmend)
    }
}
