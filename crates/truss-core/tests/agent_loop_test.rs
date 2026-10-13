//! Non-durable agent loop tests — tool loop, terminal stop, budget + max-steps
//! enforcement — driven by a scripted mock provider (no network).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use truss_core::agent::Agent;
use truss_core::budget::{BudgetLimits, BudgetMeter};
use truss_core::ctx::Ctx;
use truss_core::error::{HarnessError, ProviderError, ToolError};
use truss_core::events::AgentEvent;
use truss_core::message::Block;
use truss_core::provider::{
    CompletionRequest, CompletionResponse, Provider, StopReason, StreamEvent, ToolDef, Usage,
};
use truss_core::tools::{Tool, ToolRegistry};

struct MockProvider {
    calls: Arc<Mutex<usize>>,
    script: Vec<CompletionResponse>,
    stream_script: Option<Vec<StreamEvent>>,
    /// If true, repeat the last scripted response forever (to test max_steps).
    repeat_last: bool,
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut n = self.calls.lock().unwrap();
        let i = *n;
        *n += 1;
        if let Some(r) = self.script.get(i) {
            Ok(r.clone())
        } else if self.repeat_last {
            Ok(self.script.last().unwrap().clone())
        } else {
            Err(ProviderError::Config("script exhausted".into()))
        }
    }

    async fn complete_stream<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _req: &'a CompletionRequest,
    ) -> Result<futures::stream::BoxStream<'a, Result<StreamEvent, ProviderError>>, ProviderError>
    {
        if let Some(events) = &self.stream_script {
            Ok(Box::pin(futures::stream::iter(
                events.clone().into_iter().map(Ok),
            )))
        } else {
            let resp = self.complete(_ctx, _req).await?;
            let text = resp
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta(text)),
                Ok(StreamEvent::Done(resp)),
            ])))
        }
    }
}

struct PingTool {
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for PingTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "ping".into(),
            description: "test".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _ctx: &Ctx, _input: serde_json::Value) -> Result<String, ToolError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok("pong".into())
    }
}

fn tool_use(id: &str, name: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input: json!({}),
        }],
        stop_reason: StopReason::ToolUse,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(t.into())],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn agent(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, max_steps: usize) -> Agent {
    let mut a = Agent::new(provider, tools);
    a.max_steps = max_steps;
    a
}

#[tokio::test]
async fn runs_tool_then_finishes() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![tool_use("c1", "ping"), text("done")],
        stream_script: None,
        repeat_last: false,
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let a = agent(provider, Arc::new(reg), 8);

    let outcome = a.run(&Ctx::new(), "go").await.unwrap();
    assert_eq!(outcome.final_text, "done");
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "tool ran exactly once");
}

#[tokio::test]
async fn enforces_budget_ceiling() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![text("done")],
        stream_script: None,
        repeat_last: false,
    });
    // 5-token ceiling; the first call records 15 → post-call check trips.
    let budget = BudgetMeter::new(BudgetLimits {
        max_total_tokens: Some(5),
        ..Default::default()
    });
    let ctx = Ctx::new().with_budget(budget);
    let a = agent(provider, Arc::new(ToolRegistry::new()), 8);

    let err = a.run(&ctx, "go").await.unwrap_err();
    assert!(
        matches!(err, HarnessError::BudgetExceeded(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn stops_at_max_steps() {
    // The model never finishes (always asks for a tool) → loop hits max_steps.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![tool_use("c1", "ping")],
        stream_script: None,
        repeat_last: true,
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits }));
    let a = agent(provider, Arc::new(reg), 3);

    let err = a.run(&Ctx::new(), "go").await.unwrap_err();
    assert!(matches!(err, HarnessError::MaxSteps(3)), "got {err:?}");
}

#[tokio::test]
async fn emits_text_deltas_from_provider_stream() {
    let final_resp = text("hello world");
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![],
        stream_script: Some(vec![
            StreamEvent::TextDelta("hello ".into()),
            StreamEvent::TextDelta("world".into()),
            StreamEvent::Done(final_resp),
        ]),
        repeat_last: false,
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);
    let a = agent(provider, Arc::new(ToolRegistry::new()), 8);

    let outcome = a.run(&ctx, "go").await.unwrap();
    assert_eq!(outcome.final_text, "hello world");

    let mut deltas = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::AssistantTextDelta { text, .. } = event {
            deltas.push(text);
        }
    }
    assert_eq!(deltas, vec!["hello ", "world"]);
}

#[tokio::test]
async fn emits_context_pressure_after_each_model_step() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![text("done")],
        stream_script: None,
        repeat_last: false,
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);
    let a = agent(provider, Arc::new(ToolRegistry::new()), 8);

    a.run(&ctx, "go").await.unwrap();

    let mut pressures = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ContextPressure {
            step,
            tokens,
            window,
            pressure,
        } = event
        {
            pressures.push((step, tokens, window, pressure));
        }
    }

    assert_eq!(pressures.len(), 1);
    let (step, tokens, window, pressure) = pressures[0];
    assert_eq!(step, 1);
    assert_eq!(tokens, 10);
    assert_eq!(window, truss_core::pricing::DEFAULT_CONTEXT_WINDOW);
    assert_eq!(
        pressure,
        10.0 / truss_core::pricing::DEFAULT_CONTEXT_WINDOW as f64
    );
}
