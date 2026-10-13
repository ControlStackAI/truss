//! Hermetic CC-4 compaction-controller eval harness.
//!
//! This module deliberately observes the existing durable loop instead of adding
//! new compaction behavior. It runs one scripted workload against three
//! [`crate::memory::CompactionPolicy`] arms, then derives metrics from the
//! emitted [`crate::events::AgentEvent`] stream and signed journal entries.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;

use crate::ctx::Ctx;
use crate::durable::{self, DurableConfig, DurableOutcome};
use crate::error::{HarnessError, ProviderError, ToolError};
use crate::events::AgentEvent;
use crate::memory::{
    CompactionMode, CompactionPolicy, EffectiveBudgetPolicy, MemoryConfig, SqliteMemoryStore,
};
use crate::message::{Block, Message};
use crate::provider::{
    CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason, ToolDef, Usage,
};
use crate::store::{JournalEntry, Store};
use crate::tools::{Tool, ToolRegistry};

const DEFAULT_CONTEXT_WINDOW: u32 = 100;
const DEFAULT_THRESHOLD: f64 = 0.70;
const DEFAULT_MIN_TAIL: usize = 1;
const DEFAULT_LOOKAHEAD_STEPS: u64 = 1;
const DEFAULT_PROMPT_TOKENS: u64 = 16;
const DEFAULT_ASSISTANT_TOKENS: u64 = 3;
const DEFAULT_SUMMARY_TOKENS: u64 = 8;

/// The three CC-4 comparison arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionEvalArm {
    Off,
    Threshold,
    Anticipate,
}

impl CompactionEvalArm {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Threshold => "THRESHOLD",
            Self::Anticipate => "ANTICIPATE",
        }
    }
}

/// Deterministic scripted workload used by the hermetic eval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionEvalWorkload {
    pub name: String,
    pub context_window: u32,
    pub tool_result_token_weights: Vec<u64>,
    pub prompt_tokens: u64,
    pub assistant_tool_call_tokens: u64,
    pub context_summary_tokens: u64,
}

impl Default for CompactionEvalWorkload {
    fn default() -> Self {
        Self {
            name: "cc4_scripted_token_growth".to_string(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            tool_result_token_weights: vec![20, 20, 20, 20, 20, 20, 20],
            prompt_tokens: DEFAULT_PROMPT_TOKENS,
            assistant_tool_call_tokens: DEFAULT_ASSISTANT_TOKENS,
            context_summary_tokens: DEFAULT_SUMMARY_TOKENS,
        }
    }
}

/// Configurable eval knobs. Defaults are intentionally small and hermetic.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompactionEvalConfig {
    pub workload: CompactionEvalWorkload,
    pub threshold: f64,
    pub min_tail: usize,
    pub lookahead_steps: u64,
}

impl Default for CompactionEvalConfig {
    fn default() -> Self {
        Self {
            workload: CompactionEvalWorkload::default(),
            threshold: DEFAULT_THRESHOLD,
            min_tail: DEFAULT_MIN_TAIL,
            lookahead_steps: DEFAULT_LOOKAHEAD_STEPS,
        }
    }
}

/// Metrics for one comparison arm.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompactionArmReport {
    pub arm: CompactionEvalArm,
    pub completed: bool,
    pub completed_without_context_wall: bool,
    pub peak_pressure: f64,
    pub compaction_performed_events: usize,
    pub steps_completed: usize,
    pub total_verbatim_tail_messages_preserved: u64,
    pub tokens_summarized_away: u64,
    pub momentum_preserved_steps: usize,
}

/// Three-arm CC-4 report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompactionEvalReport {
    pub workload: CompactionEvalWorkload,
    pub arms: Vec<CompactionArmReport>,
    pub momentum_proxy: &'static str,
    pub limitations: &'static str,
}

impl CompactionEvalReport {
    /// Render a deterministic side-by-side table for PRs and local runs.
    pub fn pretty_table(&self) -> String {
        let mut out = String::from(
            "| Arm | Wall-safe | Peak pressure | Compactions | Steps completed | Momentum steps | Tail msgs preserved | Tokens summarized-away |\n\
             | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
        );
        for arm in &self.arms {
            out.push_str(&format!(
                "| {} | {} | {:.2} | {} | {} | {} | {} | {} |\n",
                arm.arm.label(),
                if arm.completed_without_context_wall {
                    "yes"
                } else {
                    "no"
                },
                arm.peak_pressure,
                arm.compaction_performed_events,
                arm.steps_completed,
                arm.momentum_preserved_steps,
                arm.total_verbatim_tail_messages_preserved,
                arm.tokens_summarized_away
            ));
        }
        out
    }
}

/// Run the default three-arm hermetic compaction eval.
pub async fn run_default_eval() -> Result<CompactionEvalReport, HarnessError> {
    run_eval(&CompactionEvalConfig::default()).await
}

/// Run all CC-4 arms against the same scripted workload.
pub async fn run_eval(cfg: &CompactionEvalConfig) -> Result<CompactionEvalReport, HarnessError> {
    let arms = vec![
        run_arm(CompactionEvalArm::Off, cfg).await?,
        run_arm(CompactionEvalArm::Threshold, cfg).await?,
        run_arm(CompactionEvalArm::Anticipate, cfg).await?,
    ];
    Ok(CompactionEvalReport {
        workload: cfg.workload.clone(),
        arms,
        momentum_proxy: "momentum_preserved_steps counts model steps completed before the first observed context_pressure event exceeded 1.0; if no overflow was observed, it equals steps_completed.",
        limitations: "This is a synthetic, scripted provider workload. It measures whether the controller preserves pressure headroom in a deterministic harness; it is not a live SWE-bench or model-quality result.",
    })
}

async fn run_arm(
    arm: CompactionEvalArm,
    cfg: &CompactionEvalConfig,
) -> Result<CompactionArmReport, HarnessError> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut ctx = Ctx::new();
    ctx = ctx.with_events(tx);
    let store = Store::open_memory().map_err(other)?;
    let memory_store = Arc::new(SqliteMemoryStore::from_store(&store).map_err(other)?);
    let provider: Arc<dyn Provider> = Arc::new(ScriptedEvalProvider::new(cfg.workload.clone()));

    let outcome = durable::run(
        provider,
        registry(),
        &ctx,
        &store,
        &format!("cc4-{}", arm.label().to_ascii_lowercase()),
        &DurableConfig {
            max_steps: cfg.workload.tool_result_token_weights.len() + 2,
            memory: MemoryConfig {
                compaction: policy_for_arm(arm, cfg),
                effective_budget: EffectiveBudgetPolicy {
                    // CC-4 predates EC-1 and remains a nominal-window baseline.
                    ratio: 1.0,
                    ..EffectiveBudgetPolicy::default()
                },
                ..MemoryConfig::default()
            },
            memory_store: Some(memory_store),
            ..DurableConfig::default()
        },
        Some("complete the scripted CC-4 workload".to_string()),
    )
    .await?;

    let events = drain_events(rx);
    let entries = store
        .entries(&format!("cc4-{}", arm.label().to_ascii_lowercase()))
        .map_err(other)?;
    Ok(report_arm(
        arm,
        matches!(outcome, DurableOutcome::Completed { .. }),
        &events,
        &entries,
    ))
}

fn policy_for_arm(arm: CompactionEvalArm, cfg: &CompactionEvalConfig) -> CompactionPolicy {
    match arm {
        CompactionEvalArm::Off => CompactionPolicy::default(),
        CompactionEvalArm::Threshold => CompactionPolicy {
            mode: CompactionMode::Summarize,
            threshold: cfg.threshold,
            min_tail: cfg.min_tail,
            anticipate: false,
            lookahead_steps: cfg.lookahead_steps,
            ..CompactionPolicy::default()
        },
        CompactionEvalArm::Anticipate => CompactionPolicy {
            mode: CompactionMode::Summarize,
            threshold: cfg.threshold,
            min_tail: cfg.min_tail,
            anticipate: true,
            lookahead_steps: cfg.lookahead_steps,
            ..CompactionPolicy::default()
        },
    }
}

fn report_arm(
    arm: CompactionEvalArm,
    completed: bool,
    events: &[AgentEvent],
    entries: &[JournalEntry],
) -> CompactionArmReport {
    let pressures = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ContextPressure { pressure, .. } => Some(*pressure),
            _ => None,
        })
        .collect::<Vec<_>>();
    let peak_pressure = pressures.iter().copied().fold(0.0_f64, f64::max);
    let steps_completed = pressures.len();
    let momentum_preserved_steps = pressures
        .iter()
        .position(|pressure| *pressure > 1.0)
        .unwrap_or(steps_completed);
    let compaction_entries = entries
        .iter()
        .filter(|entry| entry.kind == "compaction_performed")
        .collect::<Vec<_>>();
    let total_verbatim_tail_messages_preserved = compaction_entries
        .iter()
        .filter_map(|entry| entry.payload["min_tail"].as_u64())
        .sum();
    let tokens_summarized_away = compaction_entries
        .iter()
        .map(|entry| {
            let before = entry.payload["before_tokens"].as_u64().unwrap_or(0);
            let after = entry.payload["after_tokens"].as_u64().unwrap_or(before);
            before.saturating_sub(after)
        })
        .sum();

    CompactionArmReport {
        arm,
        completed,
        completed_without_context_wall: completed && peak_pressure <= 1.0,
        peak_pressure,
        compaction_performed_events: compaction_entries.len(),
        steps_completed,
        total_verbatim_tail_messages_preserved,
        tokens_summarized_away,
        momentum_preserved_steps,
    }
}

fn drain_events(mut rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(WeightedPayloadTool));
    Arc::new(registry)
}

#[derive(Clone)]
struct ScriptedEvalProvider {
    calls: Arc<Mutex<usize>>,
    workload: CompactionEvalWorkload,
}

impl ScriptedEvalProvider {
    fn new(workload: CompactionEvalWorkload) -> Self {
        Self {
            calls: Arc::new(Mutex::new(0)),
            workload,
        }
    }
}

#[async_trait]
impl Provider for ScriptedEvalProvider {
    fn name(&self) -> &str {
        "cc4-scripted"
    }

    fn default_model(&self) -> &str {
        "cc4-scripted-model"
    }

    fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities {
            context_window: self.workload.context_window,
            ..ModelCapabilities::default()
        }
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut calls = self.calls.lock().unwrap();
        let index = *calls;
        *calls += 1;
        let input_tokens = meter_request(req, &self.workload);
        let content = if let Some(weight) = self.workload.tool_result_token_weights.get(index) {
            vec![Block::ToolUse {
                id: format!("cc4-call-{}", index + 1),
                name: "weighted_payload".to_string(),
                input: json!({
                    "step": index + 1,
                    "weight": weight,
                }),
            }]
        } else {
            vec![Block::Text("scripted workload complete".to_string())]
        };
        let stop_reason = if index < self.workload.tool_result_token_weights.len() {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        Ok(CompletionResponse {
            content,
            stop_reason,
            model: "cc4-scripted-model".to_string(),
            usage: Usage {
                input_tokens,
                output_tokens: 1,
            },
        })
    }
}

fn meter_request(req: &CompletionRequest, workload: &CompactionEvalWorkload) -> u64 {
    req.messages
        .iter()
        .map(|message| message_weight(message, workload))
        .sum()
}

fn message_weight(message: &Message, workload: &CompactionEvalWorkload) -> u64 {
    message
        .content
        .iter()
        .map(|block| match block {
            Block::Text(text) if text.contains("context summary") => {
                workload.context_summary_tokens
            }
            Block::Text(_) => workload.prompt_tokens,
            Block::ToolUse { .. } => workload.assistant_tool_call_tokens,
            Block::ToolResult { content, .. } => payload_weight(content).unwrap_or(0),
        })
        .sum()
}

fn payload_weight(content: &str) -> Option<u64> {
    content
        .split_whitespace()
        .find_map(|part| part.strip_prefix("cc4_weight="))
        .and_then(|value| value.parse().ok())
}

struct WeightedPayloadTool;

#[async_trait]
impl Tool for WeightedPayloadTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "weighted_payload".to_string(),
            description: "Returns a deterministic payload with an explicit token-growth weight."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "step": { "type": "integer" },
                    "weight": { "type": "integer" }
                },
                "required": ["step", "weight"]
            }),
        }
    }

    async fn call(&self, _ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        let step = input["step"].as_u64().unwrap_or(0);
        let weight = input["weight"].as_u64().unwrap_or(0);
        Ok(format!("cc4_step={step} cc4_weight={weight}"))
    }
}

fn other<E: std::fmt::Display>(e: E) -> HarnessError {
    HarnessError::Other(e.to_string())
}
