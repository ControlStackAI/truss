use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use truss_core::ctx::Ctx;
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::{ProviderError, ToolError};
use truss_core::events::AgentEvent;
use truss_core::memory::{
    CompactionMode, CompactionPolicy, EffectiveBudgetPolicy, EntryKind, GoalLedger, MemoryConfig,
    MemoryStore, Persona, PersonaCore, PersonaSelf, SqliteMemoryStore,
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

fn cfg(mode: CompactionMode, threshold: f64, min_tail: usize) -> MemoryConfig {
    cfg_with_effective_ratio(mode, threshold, min_tail, 1.0)
}

fn cfg_with_effective_ratio(
    mode: CompactionMode,
    threshold: f64,
    min_tail: usize,
    effective_ratio: f64,
) -> MemoryConfig {
    MemoryConfig {
        compaction: CompactionPolicy {
            mode,
            threshold,
            min_tail,
            ..CompactionPolicy::default()
        },
        effective_budget: EffectiveBudgetPolicy {
            ratio: effective_ratio,
            ..EffectiveBudgetPolicy::default()
        },
        ..MemoryConfig::default()
    }
}

async fn run_script(
    memory: MemoryConfig,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    events: Option<tokio::sync::mpsc::UnboundedSender<AgentEvent>>,
) -> (Store, Arc<SqliteMemoryStore>) {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        requests,
        script: vec![
            tool_use("c1", "first-result", 10),
            tool_use("c2", "second-result", 80),
            text("done", 10),
        ],
        context_window: 100,
    });
    let store = Store::open_memory().unwrap();
    let memory_store = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    let mut ctx = Ctx::new();
    if let Some(events) = events {
        ctx = ctx.with_events(events);
    }
    let outcome = durable::run(
        provider,
        registry(),
        &ctx,
        &store,
        "cc2-session",
        &DurableConfig {
            max_steps: 8,
            memory,
            memory_store: Some(memory_store.clone()),
            goal_ledger: Some(GoalLedger::from_contract(
                "keep the goal pinned",
                vec!["finish governed compaction".into()],
                1,
            )),
            persona: Some(test_persona()),
            principal: Some("principal stays in L1".into()),
            ..DurableConfig::default()
        },
        Some("original task".into()),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DurableOutcome::Completed { .. }));
    (store, memory_store)
}

fn test_persona() -> Persona {
    let signer = truss_core::identity::Signer::generate();
    Persona {
        core: PersonaCore::signed("core identity stays in L0", &signer),
        self_: PersonaSelf {
            body: "self identity stays in L0".into(),
            last_amended_by: None,
            version: 1,
        },
    }
}

#[tokio::test]
async fn pressure_triggers_compaction_at_step_boundary() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (store, memory_store) = run_script(
        cfg(CompactionMode::Summarize, 0.70, 1),
        requests.clone(),
        Some(tx),
    )
    .await;

    let entries = store.entries("cc2-session").unwrap();
    let compaction = entries
        .iter()
        .find(|entry| entry.kind == "compaction_performed")
        .expect("signed compaction entry");
    assert_eq!(compaction.payload["before_tokens"], 80);
    assert!(compaction.payload["after_tokens"].as_u64().unwrap() < 80);

    let summary_id = compaction.payload["summary_memory_id"].as_str().unwrap();
    let summary = memory_store
        .get(&truss_core::memory::MemoryId(summary_id.to_string()))
        .unwrap();
    assert_eq!(summary.kind, EntryKind::ContextSummary);
    assert!(summary.body.contains("earlier turns elided"));

    let third_request = requests.lock().unwrap()[2].clone();
    assert!(third_request
        .messages
        .iter()
        .any(|message| message.text().contains("context summary")));
    let mut saw_event = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::CompactionPerformed { .. }) {
            saw_event = true;
        }
    }
    assert!(saw_event, "event sink received compaction event");
}

#[tokio::test]
async fn pressure_threshold_uses_effective_budget_not_nominal_window() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (store, _) = run_script(
        cfg_with_effective_ratio(CompactionMode::Summarize, 0.90, 1, 0.50),
        requests,
        Some(tx),
    )
    .await;

    let compaction = store
        .entries("cc2-session")
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "compaction_performed")
        .expect("effective budget crosses threshold even though nominal does not");
    assert_eq!(compaction.payload["before_tokens"], 80);

    let mut saw_effective_pressure = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ContextPressure {
            tokens,
            window,
            pressure,
            ..
        } = event
        {
            if tokens == 80 {
                assert_eq!(window, 50);
                assert_eq!(pressure, 1.6);
                saw_effective_pressure = true;
            }
        }
    }
    assert!(
        saw_effective_pressure,
        "pressure event used effective budget"
    );
}

#[tokio::test]
async fn exempt_layers_and_min_tail_survive_verbatim() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    run_script(
        cfg(CompactionMode::Summarize, 0.70, 2),
        requests.clone(),
        None,
    )
    .await;

    let third_request = requests.lock().unwrap()[2].clone();
    let system_text = third_request
        .system_blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>();
    assert!(system_text.contains(&"core identity stays in L0"));
    assert!(system_text.contains(&"self identity stays in L0"));
    assert!(system_text.contains(&"principal stays in L1"));
    assert!(third_request.messages[0]
        .text()
        .contains("keep the goal pinned"));

    assert!(third_request.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { content, .. } if content == "first-result"
            )
        })
    }));
    assert!(third_request.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                Block::ToolUse { id, .. } if id == "c2"
            )
        })
    }));
}

#[tokio::test]
async fn elided_range_is_recoverable_from_authoritative_journal() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = run_script(cfg(CompactionMode::Summarize, 0.70, 1), requests, None).await;

    let entries = store.entries("cc2-session").unwrap();
    let compaction = entries
        .iter()
        .find(|entry| entry.kind == "compaction_performed")
        .unwrap();
    let start = compaction.payload["elided_range"]["start"]["seq"]
        .as_i64()
        .unwrap();
    let end = compaction.payload["elided_range"]["end"]["seq"]
        .as_i64()
        .unwrap();
    let recovered = entries
        .iter()
        .filter(|entry| entry.seq >= start && entry.seq <= end)
        .map(|entry| entry.payload.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(recovered.contains("c1"));
    assert!(recovered.contains("first-result"));
}

#[tokio::test]
async fn compaction_entry_is_signed_and_chain_verifies() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (store, _) = run_script(cfg(CompactionMode::Summarize, 0.70, 1), requests, None).await;

    let entries = store.entries("cc2-session").unwrap();
    let compaction = entries
        .iter()
        .find(|entry| entry.kind == "compaction_performed")
        .unwrap();
    assert!(!compaction.signature.is_empty());
    assert_eq!(
        store.verify_chain("cc2-session").unwrap(),
        Ok(entries.len())
    );
}

#[tokio::test]
async fn disabled_or_below_threshold_has_zero_behavior_change() {
    let baseline_requests = Arc::new(Mutex::new(Vec::new()));
    let (baseline_store, _) =
        run_script(MemoryConfig::default(), baseline_requests.clone(), None).await;

    let below_requests = Arc::new(Mutex::new(Vec::new()));
    let (below_store, _) = run_script(
        cfg(CompactionMode::Summarize, 0.95, 1),
        below_requests.clone(),
        None,
    )
    .await;

    assert!(!baseline_store
        .entries("cc2-session")
        .unwrap()
        .iter()
        .any(|entry| entry.kind == "compaction_performed"));
    assert!(!below_store
        .entries("cc2-session")
        .unwrap()
        .iter()
        .any(|entry| entry.kind == "compaction_performed"));

    let baseline_messages = serde_json::to_value(&baseline_requests.lock().unwrap()[2].messages)
        .expect("serialize baseline messages");
    let below_messages = serde_json::to_value(&below_requests.lock().unwrap()[2].messages)
        .expect("serialize below-threshold messages");
    assert_eq!(baseline_messages, below_messages);
}
