//! The observable event stream.
//!
//! Everything the loop does emits an [`AgentEvent`]. This is the spine of the
//! "review proofs, not transcripts" goal: the same stream drives a live UI, a
//! durable audit log ([`crate::state`]), and (later) verdict evidence. Events are
//! `Serialize` so they can be written as JSONL verbatim.

use serde::{Deserialize, Serialize};

use crate::budget::BudgetLimits;
use crate::memory::{EntryKind, LedgerRange, LedgerRef, Trust};

/// A produced artifact referenced by verified child-run results and events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    pub path: String,
    pub kind: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Fan-out reduction strategy selected for a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FanOutReducerKind {
    Vote,
    JudgePanel,
    BestScore,
    FirstSuccess,
}

/// Parent-ledger reference for one child branch included in a fan-out reduction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FanOutChildRef {
    pub index: usize,
    pub child_run_id: String,
    pub status: String,
    pub parent_anchor: LedgerRef,
    pub verdict_ref: Option<LedgerRef>,
    pub child_head_hash: String,
}

/// The branch selected by a fan-out reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FanOutWinner {
    pub index: usize,
    pub child_run_id: String,
    pub verdict_ref: LedgerRef,
}

/// A single observable moment in an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    RunStarted {
        run_id: String,
        model: String,
    },
    StepStarted {
        step: u64,
    },
    /// Incremental assistant text produced while a provider stream is in flight.
    AssistantTextDelta {
        step: u64,
        text: String,
    },
    /// Assistant text produced on this step (may be empty if it only called tools).
    AssistantText {
        step: u64,
        text: String,
    },
    ToolUseRequested {
        step: u64,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        step: u64,
        id: String,
        name: String,
        is_error: bool,
        /// Truncated preview; the full result still goes to the model.
        preview: String,
    },
    MemoryWrite {
        id: String,
        path: String,
        kind: EntryKind,
        key: Option<String>,
        trust: Trust,
        produced_by: LedgerRef,
    },
    MemoryRecall {
        path: String,
        kind: Option<EntryKind>,
        min_trust: Trust,
        count: usize,
    },
    Usage {
        step: u64,
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: f64,
    },
    ContextPressure {
        step: u64,
        tokens: u64,
        window: u32,
        pressure: f64,
    },
    CompactionImminent {
        step: u64,
        steps_to_threshold: u64,
        pressure: f64,
        window: u32,
    },
    CompactionPerformed {
        step: u64,
        mode: String,
        before_tokens: u64,
        after_tokens: u64,
        summary_memory_id: String,
        elided_range: LedgerRange,
    },
    SelfValidation {
        cmd: String,
        exit: i32,
        summary: String,
        workdir_hash: String,
    },
    /// Emitted when spend crosses the escalation threshold (e.g. 80%).
    BudgetWarning {
        fraction_used: f64,
    },
    ChildStarted {
        parent_run_id: String,
        child_run_id: String,
        task: String,
        scope: String,
        budget_slice: BudgetLimits,
    },
    ChildCompleted {
        parent_run_id: String,
        child_run_id: String,
        verdict_ref: LedgerRef,
        child_head_hash: String,
        artifacts: Vec<ArtifactRef>,
    },
    ChildFailed {
        parent_run_id: String,
        child_run_id: String,
        error: String,
    },
    FanoutStarted {
        parent_run_id: String,
        fanout_id: String,
        task: String,
        k: usize,
        reducer: FanOutReducerKind,
    },
    FanoutCompleted {
        parent_run_id: String,
        fanout_id: String,
        reducer: FanOutReducerKind,
        child_results: Vec<FanOutChildRef>,
        winner: Option<FanOutWinner>,
        why: String,
        decision_ref: LedgerRef,
    },
    RunCompleted {
        stop_reason: String,
        steps: u64,
        total_tokens: u64,
        cost_usd: f64,
    },
    RunFailed {
        error: String,
    },
    Cancelled,
}
