use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use truss_core::budget::{BudgetLimits, BudgetMeter};
use truss_core::ctx::Ctx;
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::{ProviderError, ToolError};
use truss_core::events::AgentEvent;
use truss_core::message::Block;
use truss_core::provider::{
    CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason, Usage,
};
use truss_core::store::Store;
use truss_core::tools::ToolRegistry;

struct ScriptedProvider {
    script: Mutex<VecDeque<CompletionResponse>>,
}

impl ScriptedProvider {
    fn new(script: Vec<CompletionResponse>) -> Self {
        Self {
            script: Mutex::new(script.into()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn default_model(&self) -> &str {
        "scripted-model"
    }

    fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Decode("script exhausted".into()))
    }
}

fn tool_use(id: &str, input: Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::ToolUse {
            id: id.into(),
            name: "spawn".into(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
        model: "scripted-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 3,
        },
    }
}

fn text(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        model: "scripted-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn registry_with_spawn(
    provider: Arc<dyn Provider>,
    store: &Store,
    cfg: DurableConfig,
) -> Arc<ToolRegistry> {
    let child_tools = Arc::new(ToolRegistry::new());
    let mut registry = ToolRegistry::new();
    registry.register_spawn(provider, child_tools, store.clone(), cfg);
    Arc::new(registry)
}

#[tokio::test]
async fn model_invokes_spawn_child_run_returns_verified_result_and_parent_anchor() {
    let store = Store::open_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);
    let cfg = DurableConfig {
        max_steps: 5,
        ..Default::default()
    };
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(vec![
        tool_use(
            "spawn-1",
            json!({
                "task": "summarize the fixture",
                "scope": "repo:test",
                "budget_slice": { "max_steps": 3 }
            }),
        ),
        text("child verified summary"),
        text("parent saw child verdict"),
    ]));
    let tools = registry_with_spawn(provider.clone(), &store, cfg.clone());

    let outcome = durable::run(
        provider,
        tools,
        &ctx,
        &store,
        "parent-spawn",
        &cfg,
        Some("delegate the subtask".into()),
    )
    .await
    .expect("durable run");

    assert!(matches!(
        outcome,
        DurableOutcome::Completed { final_text } if final_text == "parent saw child verdict"
    ));

    let parent_entries = store.entries("parent-spawn").unwrap();
    let anchor = parent_entries
        .iter()
        .find(|entry| entry.kind == "child_anchor")
        .expect("parent child anchor");
    assert_eq!(anchor.payload["status"], "completed");
    let child_run_id = anchor.payload["child_run_id"].as_str().unwrap();
    assert!(store.verify_chain("parent-spawn").unwrap().is_ok());

    let tool_result = parent_entries
        .iter()
        .find(|entry| entry.kind == "tool_result" && entry.payload["name"] == "spawn")
        .expect("spawn tool result");
    let content = tool_result.payload["content"].as_str().unwrap();
    let returned: Value = serde_json::from_str(content).unwrap();
    assert_eq!(returned["child_run_id"], child_run_id);
    assert_eq!(returned["verdict"]["overall"], "Pass");
    assert_eq!(returned["verdict"]["summary"], "child verified summary");
    assert_eq!(returned["ledger"]["parent_anchor"]["seq"], anchor.seq);
    assert_eq!(
        returned["ledger"]["parent_anchor"]["session"],
        "parent-spawn"
    );

    let mut saw_started = false;
    let mut saw_completed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ChildStarted {
                parent_run_id,
                child_run_id: event_child,
                ..
            } => {
                saw_started = parent_run_id == "parent-spawn" && event_child == child_run_id;
            }
            AgentEvent::ChildCompleted {
                parent_run_id,
                child_run_id: event_child,
                verdict_ref,
                ..
            } => {
                saw_completed = parent_run_id == "parent-spawn"
                    && event_child == child_run_id
                    && verdict_ref.session == child_run_id;
            }
            _ => {}
        }
    }
    assert!(saw_started, "spawn reused child_started event");
    assert!(saw_completed, "spawn reused child_completed event");
}

#[tokio::test]
async fn spawn_tool_enforces_parent_budget_ceiling() {
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new().with_budget(BudgetMeter::new(BudgetLimits {
        max_steps: Some(1),
        ..Default::default()
    }));
    let cfg = DurableConfig {
        max_steps: 5,
        ..Default::default()
    };
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(vec![text("too costly")]));
    let tools = registry_with_spawn(provider, &store, cfg);

    let err = tools
        .call(
            &ctx,
            "spawn",
            json!({
                "task": "consume the remaining parent budget",
                "budget_slice": { "max_steps": 10 }
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ToolError::Execution(ref message) if message.contains("budget exceeded")),
        "got {err:?}"
    );
    assert_eq!(ctx.budget.snapshot().steps, 1);

    let parent_entries = store.entries(&ctx.run_id).unwrap();
    let anchor = parent_entries
        .iter()
        .find(|entry| entry.kind == "child_anchor")
        .expect("failed child anchor");
    assert_eq!(anchor.payload["status"], "failed");
    assert!(store.verify_chain(&ctx.run_id).unwrap().is_ok());
}

#[tokio::test]
async fn spawn_tool_is_default_off_and_opt_in_registered() {
    assert!(
        ToolRegistry::with_defaults()
            .defs()
            .iter()
            .all(|def| def.name != "spawn"),
        "spawn is not part of default tools"
    );

    let store = Store::open_memory().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(Vec::new()));
    let tools = registry_with_spawn(provider, &store, DurableConfig::default());
    assert!(
        tools.defs().iter().any(|def| def.name == "spawn"),
        "spawn is visible after opt-in registration"
    );
}
