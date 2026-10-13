use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use truss_core::ctx::Ctx;
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::{ProviderError, ToolError};
use truss_core::events::AgentEvent;
use truss_core::memory::{
    CompactionMode, CompactionPolicy, EffectiveBudgetPolicy, MemoryConfig, SqliteMemoryStore,
};
use truss_core::message::Block;
use truss_core::provider::{
    CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason, ToolDef, Usage,
};
use truss_core::store::Store;
use truss_core::tools::{Tool, ToolRegistry};

#[derive(Clone)]
struct MockProvider {
    calls: Arc<Mutex<usize>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    script: Vec<CompletionResponse>,
    context_window: u32,
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities {
            context_window: self.context_window,
            ..ModelCapabilities::default()
        }
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let mut calls = self.calls.lock().unwrap();
        let index = *calls;
        *calls += 1;
        self.script
            .get(index)
            .cloned()
            .ok_or_else(|| ProviderError::Config(format!("script exhausted at call {index}")))
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "echo".into(),
            description: "returns the call id payload".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }),
        }
    }

    async fn call(&self, _ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        Ok(input["value"].as_str().unwrap_or("").to_string())
    }
}

fn tool_use(id: &str, value: &str, input_tokens: u64) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::ToolUse {
            id: id.into(),
            name: "echo".into(),
            input: json!({ "value": value }),
        }],
        stop_reason: StopReason::ToolUse,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens,
            output_tokens: 1,
        },
    }
}

fn text(text: &str, input_tokens: u64) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens,
            output_tokens: 1,
        },
    }
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    Arc::new(registry)
}

fn compaction_config(threshold: f64, anticipate: bool, lookahead_steps: u64) -> MemoryConfig {
    compaction_config_with_effective_ratio(threshold, anticipate, lookahead_steps, 1.0)
}

fn compaction_config_with_effective_ratio(
    threshold: f64,
    anticipate: bool,
    lookahead_steps: u64,
    effective_ratio: f64,
) -> MemoryConfig {
    MemoryConfig {
        compaction: CompactionPolicy {
            mode: CompactionMode::Summarize,
            threshold,
            min_tail: 1,
            anticipate,
            lookahead_steps,
            ..CompactionPolicy::default()
        },
        effective_budget: EffectiveBudgetPolicy {
            ratio: effective_ratio,
            ..EffectiveBudgetPolicy::default()
        },
        ..MemoryConfig::default()
    }
}

fn scripted_responses(input_tokens: &[u64]) -> Vec<CompletionResponse> {
    input_tokens
        .iter()
        .enumerate()
        .map(|(index, tokens)| {
            if index + 1 == input_tokens.len() {
                text("done", *tokens)
            } else {
                tool_use(
                    &format!("c{}", index + 1),
                    &format!("result-{}", index + 1),
                    *tokens,
                )
            }
        })
        .collect()
}

async fn run_script(
    input_tokens: &[u64],
    memory: MemoryConfig,
    events: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
    memory_store_enabled: bool,
) -> Store {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
        script: scripted_responses(input_tokens),
        context_window: 100,
    });
    let store = Store::open_memory().unwrap();
    let memory_store = memory_store_enabled
        .then(|| Arc::new(SqliteMemoryStore::from_store(&store).unwrap()) as Arc<_>);
    let mut ctx = Ctx::new();
    if let Some(events) = events {
        ctx = ctx.with_events(events);
    }

    let outcome = durable::run(
        provider,
        registry(),
        &ctx,
        &store,
        "cc3-session",
        &DurableConfig {
            max_steps: 8,
            memory,
            memory_store,
            ..DurableConfig::default()
        },
        Some("original task".into()),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DurableOutcome::Completed { .. }));
    store
}

fn drain_events(mut rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn compaction_imminent_fires_within_horizon() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    run_script(
        &[50, 60, 62],
        compaction_config(0.70, true, 1),
        Some(tx),
        false,
    )
    .await;

    let imminent = drain_events(rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::CompactionImminent {
                steps_to_threshold,
                pressure,
                window,
                ..
            } => Some((steps_to_threshold, pressure, window)),
            _ => None,
        })
        .expect("compaction_imminent event");

    assert_eq!(imminent.0, 1);
    assert_eq!(imminent.1, 0.60);
    assert_eq!(imminent.2, 100);
}

#[tokio::test]
async fn compaction_imminent_uses_effective_budget_window() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    run_script(
        &[30, 35, 40],
        compaction_config_with_effective_ratio(0.90, true, 1, 0.50),
        Some(tx),
        false,
    )
    .await;

    let imminent = drain_events(rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::CompactionImminent {
                steps_to_threshold,
                pressure,
                window,
                ..
            } => Some((steps_to_threshold, pressure, window)),
            _ => None,
        })
        .expect("compaction_imminent event");

    assert_eq!(imminent.0, 1);
    assert!((imminent.1 - 0.80).abs() < f64::EPSILON);
    assert_eq!(imminent.2, 50);
}

#[tokio::test]
async fn compaction_imminent_does_not_fire_outside_horizon() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    run_script(
        &[40, 50, 55],
        compaction_config(0.80, true, 1),
        Some(tx),
        false,
    )
    .await;

    assert!(!drain_events(rx)
        .into_iter()
        .any(|event| matches!(event, AgentEvent::CompactionImminent { .. })));
}

#[tokio::test]
async fn anticipate_on_compacts_one_boundary_before_threshold() {
    let store = run_script(&[50, 60, 72], compaction_config(0.70, true, 1), None, true).await;

    let entries = store.entries("cc3-session").unwrap();
    let compaction = entries
        .iter()
        .find(|entry| entry.kind == "compaction_performed")
        .expect("anticipatory compaction entry");

    assert_eq!(compaction.payload["before_tokens"], 60);
    assert!(!compaction.signature.is_empty());
    assert_eq!(
        store.verify_chain("cc3-session").unwrap(),
        Ok(entries.len())
    );
}

#[tokio::test]
async fn anticipate_off_preserves_cc2_threshold_behavior() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let store = run_script(
        &[50, 60, 72],
        compaction_config(0.70, false, 1),
        Some(tx),
        true,
    )
    .await;

    assert!(!drain_events(rx)
        .into_iter()
        .any(|event| matches!(event, AgentEvent::CompactionImminent { .. })));

    let entries = store.entries("cc3-session").unwrap();
    let compaction = entries
        .iter()
        .find(|entry| entry.kind == "compaction_performed")
        .expect("threshold compaction entry");
    assert_eq!(compaction.payload["before_tokens"], 72);
}
