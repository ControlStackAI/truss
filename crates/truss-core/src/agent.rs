//! The agent loop — the thing that makes this a *harness* and not just an API call.
//!
//! Stateless API in, stateful conversation out: we keep the full message history
//! locally, send it on every call, and when the model returns `stop_reason:
//! tool_use` we run the requested tools, append their results, and call again.
//!
//! v0.2 wraps that core loop with the things an enterprise run actually needs,
//! all flowing through one [`Ctx`]:
//! * cancellation + deadline checks at every boundary,
//! * automatic retries on transient provider errors,
//! * budget metering + enforcement (tokens / cost / steps) with escalation,
//! * an [`AgentEvent`] emitted for everything (drives UI + audit log),
//! * **parallel** tool execution when the model requests several at once.

use std::sync::Arc;

use futures::{future::join_all, StreamExt};

use crate::budget::BudgetSnapshot;
use crate::context_gauge::ContextGauge;
use crate::ctx::Ctx;
use crate::error::HarnessError;
use crate::events::AgentEvent;
use crate::memory::context_pressure_window;
use crate::message::{Block, Message, Role};
use crate::pricing;
use crate::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, StreamEvent};
use crate::retry::RetryPolicy;
use crate::tools::ToolRegistry;

/// The result of a completed run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub final_text: String,
    pub stop_reason: StopReason,
    pub usage: BudgetSnapshot,
    pub run_id: String,
}

pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<ToolRegistry>,
    pub system: Option<String>,
    pub model: Option<String>,
    pub max_tokens: u32,
    pub max_steps: usize,
    pub temperature: Option<f32>,
    pub reasoning: Option<crate::provider::ReasoningEffort>,
    pub retry: RetryPolicy,
    /// Budget fraction (0.0–1.0) at which to emit a one-shot warning. 0 disables.
    pub escalate_at: f64,
}

impl Agent {
    /// Construct with sensible defaults; override fields as needed.
    pub fn new(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>) -> Self {
        Agent {
            provider,
            tools,
            system: None,
            model: None,
            max_tokens: 4096,
            max_steps: 24,
            temperature: None,
            reasoning: None,
            retry: RetryPolicy::default(),
            escalate_at: 0.8,
        }
    }

    /// Run one prompt to completion, executing tool calls along the way.
    pub async fn run(
        &self,
        ctx: &Ctx,
        prompt: impl Into<String>,
    ) -> Result<RunOutcome, HarnessError> {
        let model = self
            .model
            .clone()
            .unwrap_or_else(|| self.provider.default_model().to_string());
        ctx.emit(AgentEvent::RunStarted {
            run_id: ctx.run_id.clone(),
            model: model.clone(),
        });

        match self.run_inner(ctx, prompt.into(), &model).await {
            Ok(outcome) => {
                ctx.emit(AgentEvent::RunCompleted {
                    stop_reason: format!("{:?}", outcome.stop_reason),
                    steps: outcome.usage.steps,
                    total_tokens: outcome.usage.total_tokens(),
                    cost_usd: outcome.usage.cost_usd,
                });
                Ok(outcome)
            }
            Err(HarnessError::Cancelled) => {
                ctx.emit(AgentEvent::Cancelled);
                Err(HarnessError::Cancelled)
            }
            Err(e) => {
                ctx.emit(AgentEvent::RunFailed {
                    error: e.to_string(),
                });
                Err(e)
            }
        }
    }

    async fn run_inner(
        &self,
        ctx: &Ctx,
        prompt: String,
        model: &str,
    ) -> Result<RunOutcome, HarnessError> {
        let mut messages = vec![Message::user(prompt)];
        let mut warned = false;

        for step in 1..=self.max_steps as u64 {
            ctx.check()?;
            ctx.budget.check()?;
            ctx.emit(AgentEvent::StepStarted { step });

            let req = CompletionRequest {
                system: self.system.clone(),
                system_blocks: Vec::new(),
                messages: messages.clone(),
                tools: self.tools.defs_for(ctx),
                max_tokens: self.max_tokens,
                model: Some(model.to_string()),
                temperature: self.temperature,
                reasoning: self.reasoning,
            };

            // Provider stream creation gets normal retry semantics. Once bytes
            // begin arriving we do not retry mid-stream because partial output
            // may already have reached the UI.
            let resp = self.complete_step_streaming(ctx, step, &req).await?;

            // Account for spend.
            let cost = pricing::cost_usd(
                &resp.model,
                resp.usage.input_tokens,
                resp.usage.output_tokens,
            );
            ctx.budget
                .record_usage(resp.usage.input_tokens, resp.usage.output_tokens, cost);
            ctx.budget.record_step();
            ctx.emit(AgentEvent::Usage {
                step,
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
                cost_usd: cost,
            });
            let pressure = ContextGauge::new(context_pressure_window(
                self.provider.as_ref(),
                Some(&resp.model),
                None,
            ))
            .measure(resp.usage.input_tokens);
            ctx.emit(AgentEvent::ContextPressure {
                step,
                tokens: pressure.tokens,
                window: pressure.window,
                pressure: pressure.pressure,
            });
            if !warned && self.escalate_at > 0.0 && ctx.budget.fraction_used() >= self.escalate_at {
                warned = true;
                ctx.emit(AgentEvent::BudgetWarning {
                    fraction_used: ctx.budget.fraction_used(),
                });
            }
            ctx.budget.check()?; // stop if this call pushed us over

            let assistant_text = Message {
                role: Role::Assistant,
                content: resp.content.clone(),
            }
            .text();
            if !assistant_text.is_empty() {
                ctx.emit(AgentEvent::AssistantText {
                    step,
                    text: assistant_text.clone(),
                });
            }

            messages.push(Message {
                role: Role::Assistant,
                content: resp.content.clone(),
            });

            if resp.stop_reason == StopReason::ToolUse {
                let results = self.run_tools(ctx, step, &resp.content).await?;
                messages.push(Message {
                    role: Role::Tool,
                    content: results,
                });
                continue;
            }

            // Terminal stop reason — done.
            return Ok(RunOutcome {
                final_text: assistant_text,
                stop_reason: resp.stop_reason,
                usage: ctx.budget.snapshot(),
                run_id: ctx.run_id.clone(),
            });
        }

        Err(HarnessError::MaxSteps(self.max_steps))
    }

    async fn complete_step_streaming(
        &self,
        ctx: &Ctx,
        step: u64,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, HarnessError> {
        let mut stream = crate::retry::with_retry(ctx, &self.retry, |_attempt| {
            self.provider.complete_stream(ctx, req)
        })
        .await
        .map_err(|e| match e {
            crate::error::ProviderError::Cancelled => HarnessError::Cancelled,
            other => HarnessError::Provider(other),
        })?;

        while let Some(event) = stream.next().await {
            match event.map_err(|e| match e {
                crate::error::ProviderError::Cancelled => HarnessError::Cancelled,
                other => HarnessError::Provider(other),
            })? {
                StreamEvent::TextDelta(text) => {
                    if !text.is_empty() {
                        ctx.emit(AgentEvent::AssistantTextDelta { step, text });
                    }
                }
                StreamEvent::Done(resp) => return Ok(resp),
            }
        }

        Err(HarnessError::Provider(crate::error::ProviderError::Decode(
            "stream ended before final response".into(),
        )))
    }

    /// Execute every tool the model requested on this step, concurrently.
    async fn run_tools(
        &self,
        ctx: &Ctx,
        step: u64,
        content: &[Block],
    ) -> Result<Vec<Block>, HarnessError> {
        ctx.check()?;

        let calls: Vec<(String, String, serde_json::Value)> = content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();

        for (id, name, input) in &calls {
            ctx.emit(AgentEvent::ToolUseRequested {
                step,
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }

        let futures = calls.into_iter().map(|(id, name, input)| async move {
            let (content, is_error) = match self.tools.call(ctx, &name, input).await {
                Ok(out) => (out, false),
                Err(e) => (format!("error: {e}"), true),
            };
            (id, name, content, is_error)
        });

        let results = join_all(futures).await;

        let mut blocks = Vec::with_capacity(results.len());
        for (id, name, content, is_error) in results {
            ctx.emit(AgentEvent::ToolResult {
                step,
                id: id.clone(),
                name,
                is_error,
                preview: preview(&content),
            });
            blocks.push(Block::ToolResult {
                id,
                content,
                is_error,
            });
        }
        Ok(blocks)
    }
}

fn preview(s: &str) -> String {
    const MAX: usize = 400;
    if s.len() <= MAX {
        s.to_string()
    } else {
        let mut end = MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… ({} bytes)", &s[..end], s.len())
    }
}
