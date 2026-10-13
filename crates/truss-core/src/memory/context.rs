//! M4 layered context assembly.
//!
//! The assembler keeps slow-changing L0/L1 in the cached system prefix and puts
//! all volatile state in messages: L3 goal ledger, projected history, L2 deltas,
//! and L4 scratch. This preserves the provider cache key when only L3/L4 churn.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::message::Message;
use crate::provider::{Provider, SystemBlock, ToolDef};

use super::persona::Persona;
use super::policy::MemoryRecallMode;
use super::store::{MemoryError, MemoryStore};
use super::types::{EntryKind, MemoryCursor, MemoryDelta, MemoryEntry, MemoryQuery, Trust};

/// Verified lesson recall selected for L2 injection at run start.
#[derive(Debug, Clone)]
pub struct LessonRecall {
    pub entries: Vec<MemoryEntry>,
    pub approx_tokens: usize,
}

/// Verified playbook recall selected for L2 injection at run start.
#[derive(Debug, Clone)]
pub struct PlaybookRecall {
    pub entries: Vec<MemoryEntry>,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone)]
enum L2ContextBlock {
    Delta(MemoryDelta),
    VerifiedLessons(LessonRecall),
    VerifiedPlaybooks(PlaybookRecall),
}

/// Context lifetime tier. Used by projection policy so compaction exemptions are
/// declarative instead of hard-coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTier {
    L0Core,
    L0Self,
    L1Principal,
    L2Memory,
    L3Goal,
    L4Scratch,
    History,
}

/// Active task intent and definition of done. This is L3: pinned at the message
/// head on cadence and never compacted into the history summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalLedger {
    pub intent: String,
    #[serde(default)]
    pub definition_of_done: Vec<String>,
    #[serde(default = "default_repin_every")]
    pub repin_every: usize,
}

impl GoalLedger {
    pub fn from_contract(intent: impl Into<String>, dod: Vec<String>, repin_every: usize) -> Self {
        Self {
            intent: intent.into(),
            definition_of_done: dod,
            repin_every: repin_every.max(1),
        }
    }

    pub fn should_pin(&self, step: usize) -> bool {
        step == 1 || step.is_multiple_of(self.repin_every.max(1))
    }

    pub fn render(&self) -> String {
        let mut out = String::from("[L3 goal ledger]\nIntent:\n");
        out.push_str(&self.intent);
        if !self.definition_of_done.is_empty() {
            out.push_str("\n\nDefinition of done:");
            for clause in &self.definition_of_done {
                out.push_str("\n- ");
                out.push_str(clause);
            }
        }
        out
    }
}

fn default_repin_every() -> usize {
    8
}

/// EC-1 pressure denominator for the Compaction Controller gauge.
///
/// Default behavior remains off-equivalent: callers without enabled governed
/// compaction keep measuring against the nominal provider window. Once
/// compaction is enabled, pressure is measured against the resolved effective
/// budget so threshold and compact-ahead decisions fire before the degraded
/// attention band.
pub fn context_pressure_window(
    provider: &dyn Provider,
    model_id: Option<&str>,
    config: Option<&super::policy::MemoryConfig>,
) -> u32 {
    let nominal = provider.context_window(model_id).max(1) as usize;
    let window = match config {
        Some(config) if config.compaction.enabled() => {
            config.effective_budget.resolve(model_id, nominal)
        }
        _ => nominal,
    };
    window.clamp(1, u32::MAX as usize) as u32
}

/// Resolved knobs that control context assembly/projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayeredContextConfig {
    #[serde(default = "default_repin_every")]
    pub goal_repin_every: usize,
    #[serde(default)]
    pub compaction_exempt_tiers: Vec<ContextTier>,
}

impl Default for LayeredContextConfig {
    fn default() -> Self {
        Self {
            goal_repin_every: default_repin_every(),
            compaction_exempt_tiers: vec![
                ContextTier::L0Core,
                ContextTier::L0Self,
                ContextTier::L1Principal,
                ContextTier::L3Goal,
            ],
        }
    }
}

/// Output of L0-L4 assembly. `legacy_system` is set only when no layered L0/L1
/// blocks are present so old callers keep identical request shape.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    pub legacy_system: Option<String>,
    pub next_memory_cursor: MemoryCursor,
    pub stable_prefix_key: String,
}

/// Builder for the M4 prompt layout.
#[derive(Debug, Clone)]
pub struct ContextAssembler {
    persona: Option<Persona>,
    principal: Option<String>,
    legacy_system: Option<String>,
    history: Vec<Message>,
    goal_ledger: Option<GoalLedger>,
    l2_blocks: Vec<L2ContextBlock>,
    memory_cursor: MemoryCursor,
    scratch: Option<String>,
    step: usize,
    use_legacy_system_when_empty: bool,
    tool_fingerprint: Vec<String>,
}

impl ContextAssembler {
    pub fn new(
        persona: Option<Persona>,
        principal: Option<String>,
        legacy_system: Option<String>,
    ) -> Self {
        Self {
            persona,
            principal,
            legacy_system,
            history: Vec::new(),
            goal_ledger: None,
            l2_blocks: Vec::new(),
            memory_cursor: MemoryCursor(0),
            scratch: None,
            step: 1,
            use_legacy_system_when_empty: false,
            tool_fingerprint: Vec::new(),
        }
    }

    pub fn tools(mut self, tools: &[ToolDef]) -> Self {
        self.tool_fingerprint = tools
            .iter()
            .map(|tool| {
                format!(
                    "{}:{}:{}",
                    tool.name,
                    tool.description,
                    serde_json::to_string(&tool.input_schema).unwrap_or_default()
                )
            })
            .collect();
        self.tool_fingerprint.sort();
        self
    }

    pub fn history(mut self, history: Vec<Message>) -> Self {
        self.history = history;
        self
    }

    pub fn goal_ledger(mut self, goal_ledger: Option<GoalLedger>) -> Self {
        self.goal_ledger = goal_ledger;
        self
    }

    pub fn memory_delta(mut self, memory_delta: Option<MemoryDelta>, cursor: MemoryCursor) -> Self {
        if let Some(delta) = memory_delta {
            self.l2_blocks.push(L2ContextBlock::Delta(delta));
        }
        self.memory_cursor = cursor;
        self
    }

    pub fn verified_lesson_recall(mut self, recall: Option<LessonRecall>) -> Self {
        if let Some(recall) = recall {
            if !recall.entries.is_empty() {
                self.l2_blocks.push(L2ContextBlock::VerifiedLessons(recall));
            }
        }
        self
    }

    pub fn verified_playbook_recall(mut self, recall: Option<PlaybookRecall>) -> Self {
        if let Some(recall) = recall {
            if !recall.entries.is_empty() {
                self.l2_blocks
                    .push(L2ContextBlock::VerifiedPlaybooks(recall));
            }
        }
        self
    }

    pub fn scratch(mut self, scratch: Option<String>) -> Self {
        self.scratch = scratch;
        self
    }

    pub fn step(mut self, step: usize) -> Self {
        self.step = step.max(1);
        self
    }

    pub fn use_legacy_system_when_empty(mut self, enabled: bool) -> Self {
        self.use_legacy_system_when_empty = enabled;
        self
    }

    pub fn assemble(self) -> AssembledContext {
        let mut system = Vec::new();
        if let Some(persona) = &self.persona {
            system.push(SystemBlock::new(persona.core.body.clone()));
            system.push(SystemBlock::new(persona.self_.body.clone()));
        }
        if let Some(principal) = &self.principal {
            if !principal.trim().is_empty() {
                system.push(SystemBlock::new(principal.clone()));
            }
        }
        if let Some(system_text) = &self.legacy_system {
            if !system_text.trim().is_empty() && !self.use_legacy_system_when_empty {
                system.push(SystemBlock::new(system_text.clone()));
            }
        }
        if let Some(last) = system.last_mut() {
            last.cache_control = Some(crate::provider::CacheControl::Ephemeral);
        }
        let stable_prefix_key = stable_key(&self.tool_fingerprint, &system);

        let mut messages = Vec::new();
        if let Some(goal) = &self.goal_ledger {
            if goal.should_pin(self.step) {
                messages.push(Message::user(goal.render()));
            }
        }
        messages.extend(self.history);
        let mut next_memory_cursor = self.memory_cursor;
        for block in self.l2_blocks {
            match block {
                L2ContextBlock::Delta(delta) if !delta.entries.is_empty() => {
                    messages.push(Message::user(render_memory_delta(&delta)));
                    next_memory_cursor = delta.cursor;
                }
                L2ContextBlock::Delta(delta) => {
                    next_memory_cursor = delta.cursor;
                }
                L2ContextBlock::VerifiedLessons(recall) if !recall.entries.is_empty() => {
                    messages.push(Message::user(render_verified_lessons(&recall)));
                }
                L2ContextBlock::VerifiedLessons(_) => {}
                L2ContextBlock::VerifiedPlaybooks(recall) if !recall.entries.is_empty() => {
                    messages.push(Message::user(render_verified_playbooks(&recall)));
                }
                L2ContextBlock::VerifiedPlaybooks(_) => {}
            }
        }
        if let Some(scratch) = self.scratch {
            if !scratch.trim().is_empty() {
                messages.push(Message::user(format!("[L4 scratch]\n{scratch}")));
            }
        }

        let legacy_system = if system.is_empty() && self.use_legacy_system_when_empty {
            self.legacy_system
        } else {
            None
        };

        AssembledContext {
            system,
            messages,
            legacy_system,
            next_memory_cursor,
            stable_prefix_key,
        }
    }
}

fn stable_key(tool_fingerprint: &[String], blocks: &[SystemBlock]) -> String {
    let mut h = Sha256::new();
    for tool in tool_fingerprint {
        h.update(b"tool:");
        h.update(tool.as_bytes());
        h.update([0xfe]);
    }
    for block in blocks {
        h.update(block.text.as_bytes());
        h.update([0]);
        h.update(if block.cache_control.is_some() {
            b"cache"
        } else {
            b"plain"
        });
        h.update([0xff]);
    }
    format!("{:x}", h.finalize())
}

fn render_memory_delta(delta: &MemoryDelta) -> String {
    let mut out = format!(
        "[L2 memory delta since cursor]\nnext_cursor: {}",
        delta.cursor.0
    );
    render_memory_entries(&mut out, &delta.entries);
    out
}

fn render_verified_lessons(recall: &LessonRecall) -> String {
    let mut out = format!(
        "[L2 verified learned lessons]\napprox_tokens: {}",
        recall.approx_tokens
    );
    render_memory_entries(&mut out, &recall.entries);
    out
}

fn render_verified_playbooks(recall: &PlaybookRecall) -> String {
    let mut out = format!(
        "[L2 verified playbooks]\napprox_tokens: {}",
        recall.approx_tokens
    );
    render_memory_entries(&mut out, &recall.entries);
    out
}

fn render_memory_entries(out: &mut String, entries: &[MemoryEntry]) {
    for entry in entries {
        out.push_str("\n- ");
        out.push_str(&format!("{:?}", entry.kind).to_ascii_lowercase());
        if let Some(key) = &entry.key {
            out.push_str(" / ");
            out.push_str(key);
        }
        out.push_str(" [trust: ");
        out.push_str(&format!("{:?}", entry.trust).to_ascii_lowercase());
        out.push_str("]\n  ");
        out.push_str(&entry.body.replace('\n', "\n  "));
        out.push_str("\n  provenance: produced_by ");
        render_ledger_ref(out, &entry.provenance.produced_by);
        if let Some(verified_by_ref) = &entry.provenance.verified_by_ref {
            out.push_str("; verified_by ");
            render_ledger_ref(out, verified_by_ref);
        }
    }
}

fn render_ledger_ref(out: &mut String, ledger: &super::types::LedgerRef) {
    out.push_str(&ledger.session);
    out.push('#');
    out.push_str(&ledger.seq.to_string());
}

/// Recall relevant, verified lessons for L2 injection. Exact recall preserves
/// the original lexical overlap path; semantic recall delegates ranking to
/// `MemoryStore::query` behind the same verified-only query shape.
pub fn recall_verified_lessons(
    store: &dyn MemoryStore,
    task_context: &str,
    config: &super::policy::MemoryConfig,
) -> Result<Option<LessonRecall>, MemoryError> {
    if config.lesson_recall_max_lessons == 0 || config.lesson_recall_token_budget == 0 {
        return Ok(None);
    }

    recall_verified_entries(
        store,
        config,
        EntryKind::Lesson,
        task_context,
        config.lesson_recall_max_lessons,
        config.lesson_recall_token_budget,
    )
    .map(|recall| {
        recall.map(|(entries, approx_tokens)| LessonRecall {
            entries,
            approx_tokens,
        })
    })
}

/// Recall relevant, verified playbooks for L2 injection. Defaults keep this off
/// until operators opt in with nonzero playbook recall limits.
pub fn recall_verified_playbooks(
    store: &dyn MemoryStore,
    task_context: &str,
    config: &super::policy::MemoryConfig,
) -> Result<Option<PlaybookRecall>, MemoryError> {
    if config.playbook_recall_max_playbooks == 0 || config.playbook_recall_token_budget == 0 {
        return Ok(None);
    }

    recall_verified_entries(
        store,
        config,
        EntryKind::Playbook,
        task_context,
        config.playbook_recall_max_playbooks,
        config.playbook_recall_token_budget,
    )
    .map(|recall| {
        recall.map(|(entries, approx_tokens)| PlaybookRecall {
            entries,
            approx_tokens,
        })
    })
}

fn recall_verified_entries(
    store: &dyn MemoryStore,
    config: &super::policy::MemoryConfig,
    kind: EntryKind,
    task_context: &str,
    max_entries: usize,
    token_budget: usize,
) -> Result<Option<(Vec<MemoryEntry>, usize)>, MemoryError> {
    if max_entries == 0 || token_budget == 0 {
        return Ok(None);
    }

    let semantic = matches!(config.recall, MemoryRecallMode::Semantic);
    let candidates = store.query(&MemoryQuery {
        kind: Some(kind),
        key: None,
        min_trust: Trust::Verified,
        contains: semantic.then(|| task_context.to_string()),
        limit: max_entries.saturating_mul(4).max(16),
    })?;
    if candidates.is_empty() {
        return Ok(None);
    }

    let ranked: Vec<MemoryEntry> = if semantic {
        candidates
            .into_iter()
            .filter(|entry| entry.trust == Trust::Verified)
            .collect()
    } else {
        let mut scored: Vec<(usize, MemoryEntry)> = candidates
            .into_iter()
            .filter(|entry| entry.trust == Trust::Verified)
            .filter_map(|entry| {
                let score = relevance_score(task_context, &entry);
                (score > 0).then_some((score, entry))
            })
            .collect();
        if scored.is_empty() {
            return Ok(None);
        }
        scored.sort_by(|(a_score, a_entry), (b_score, b_entry)| {
            b_score
                .cmp(a_score)
                .then_with(|| {
                    b_entry
                        .salience
                        .partial_cmp(&a_entry.salience)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| b_entry.created_ms.cmp(&a_entry.created_ms))
        });
        scored.into_iter().map(|(_, entry)| entry).collect()
    };

    let mut entries = Vec::new();
    let mut approx_tokens = 0usize;
    for entry in ranked {
        let entry_tokens = approx_entry_tokens(&entry);
        if entries.len() >= max_entries {
            break;
        }
        if approx_tokens + entry_tokens > token_budget {
            if entries.is_empty() {
                break;
            }
            continue;
        }
        approx_tokens += entry_tokens;
        entries.push(entry);
    }

    if entries.is_empty() {
        Ok(None)
    } else {
        Ok(Some((entries, approx_tokens)))
    }
}

fn relevance_score(task_context: &str, entry: &MemoryEntry) -> usize {
    let task_terms = terms(task_context);
    if task_terms.is_empty() {
        return 0;
    }
    let mut lesson_text = entry.body.clone();
    if let Some(key) = &entry.key {
        lesson_text.push(' ');
        lesson_text.push_str(key);
    }
    let lesson_terms = terms(&lesson_text);
    task_terms
        .iter()
        .filter(|term| lesson_terms.iter().any(|lesson| lesson == *term))
        .count()
}

fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= 3)
        .collect()
}

fn approx_entry_tokens(entry: &MemoryEntry) -> usize {
    let key_len = entry.key.as_ref().map(String::len).unwrap_or(0);
    ((entry.body.len() + key_len) / 4).max(1)
}
