//! The single abstraction every backend implements.
//!
//! A [`Provider`] is still "one request in, one response out" — statelessness
//! lives here, exactly like the real HTTP APIs. What v0.2 adds:
//!
//! * every call takes a [`Ctx`] (cancellation, deadline, principal, budget),
//! * errors are the typed [`ProviderError`] so retries can be automatic,
//! * providers declare [`ModelCapabilities`] so the loop can adapt per model,
//! * a `complete_stream` contract provides provider-neutral token streaming.
//!
//! The agent loop in [`crate::agent`] turns this single call into a multi-step,
//! tool-using conversation.

use async_trait::async_trait;
use futures::stream::{self, BoxStream};

use crate::ctx::Ctx;
use crate::error::ProviderError;
use crate::message::{Block, Message};
use crate::pricing::DEFAULT_CONTEXT_WINDOW;

/// Provider-neutral cache hint for prompt segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheControl {
    /// Cache up to this block when the provider supports an ephemeral prompt
    /// cache breakpoint.
    Ephemeral,
}

/// One system-prompt block. Providers that support native block arrays can keep
/// cache metadata; providers that only support a string render blocks together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemBlock {
    pub text: String,
    pub cache_control: Option<CacheControl>,
}

impl SystemBlock {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: None,
        }
    }

    pub fn cached(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: Some(CacheControl::Ephemeral),
        }
    }
}

/// A tool the model may call. `input_schema` is a JSON Schema object; each
/// provider translates it to its own tool-definition wire format.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Normalized reasoning ("thinking") level. This is a *lowest-common-denominator*
/// abstraction — providers don't share one scheme, so each provider's `complete`
/// maps it to that provider's own knob:
///
/// * **OpenAI** → `reasoning_effort` (the effort string).
/// * **Anthropic** → extended-thinking `budget_tokens` (a token budget).
/// * future **xAI Grok** → `reasoning_effort` but only `low`/`high` (clamp the rest);
///   **Gemini** → a thinking budget.
///
/// **Not every model supports every level** (e.g. OpenAI's `minimal` is gpt-5 only;
/// o-series has no `minimal`). When a model rejects the level, the provider's HTTP
/// call returns a typed [`crate::error::ProviderError::Status`] (a 4xx) — which is
/// **fatal and not retried**, and surfaced to the caller. A provider should clamp
/// to its supported set where it can, and otherwise let the API be the source of
/// truth rather than guessing model capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// OpenAI-style effort string (`reasoning_effort`).
    pub fn as_openai_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }

    /// Anthropic-style thinking budget in tokens (extended thinking).
    pub fn anthropic_budget_tokens(self) -> u32 {
        match self {
            ReasoningEffort::Minimal => 1024,
            ReasoningEffort::Low => 4096,
            ReasoningEffort::Medium => 8192,
            ReasoningEffort::High => 16384,
        }
    }
}

/// Everything needed for a single completion call.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: Option<String>,
    pub system_blocks: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    /// Overrides the provider's default model when set.
    pub model: Option<String>,
    pub temperature: Option<f32>,
    /// Reasoning/thinking level for models that support it.
    pub reasoning: Option<ReasoningEffort>,
}

impl CompletionRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        CompletionRequest {
            system: None,
            system_blocks: Vec::new(),
            messages,
            tools: Vec::new(),
            max_tokens: 4096,
            model: None,
            temperature: None,
            reasoning: None,
        }
    }
}

/// Why the model stopped — drives the agent loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other(String),
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One model response.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: Vec<Block>,
    pub stop_reason: StopReason,
    pub model: String,
    pub usage: Usage,
}

/// What a given model/provider can do, so the loop adapts instead of guessing.
#[derive(Debug, Clone)]
pub struct ModelCapabilities {
    pub tools: bool,
    pub vision: bool,
    pub parallel_tool_calls: bool,
    pub streaming: bool,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        ModelCapabilities {
            tools: true,
            vision: false,
            parallel_tool_calls: true,
            streaming: false,
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_output_tokens: 4096,
        }
    }
}

/// Incremental output for streaming consumers.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    /// The provider has finished; carries the assembled final response.
    Done(CompletionResponse),
}

/// Implement this once per backend.
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// The model used when [`CompletionRequest::model`] is `None`.
    fn default_model(&self) -> &str;

    /// Capabilities for `model` (or the default model when `None`).
    fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    /// Context-window size in tokens for `model` (or the default model when
    /// `None`). Unknown models should return a conservative safe default.
    fn context_window(&self, model: Option<&str>) -> u32 {
        self.capabilities(model).context_window
    }

    /// EC-1 effective context budget for the provider default model.
    ///
    /// This is intentionally smaller than the nominal context window by
    /// default: it is the budget Truss should treat as "useful attention"
    /// before governed context management intervenes.
    fn effective_context_budget(&self) -> usize {
        self.effective_context_budget_for(None)
    }

    /// EC-1 effective context budget for `model` (or the default model when
    /// `None`). Providers with measured per-model budgets may override this.
    fn effective_context_budget_for(&self, model: Option<&str>) -> usize {
        crate::memory::EffectiveBudgetPolicy::default()
            .resolve(model, self.context_window(model) as usize)
    }

    /// One blocking completion. The implementation must honor `ctx` cancellation
    /// (race the HTTP call against `ctx.cancel_token().cancelled()`).
    async fn complete(
        &self,
        ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;

    /// Token-level streaming. Default implementation runs `complete` and emits a
    /// single text delta + `Done`, so every backend "streams" correctly even
    /// before it implements real SSE. Override for true token streaming.
    async fn complete_stream<'a>(
        &'a self,
        ctx: &'a Ctx,
        req: &'a CompletionRequest,
    ) -> Result<BoxStream<'a, Result<StreamEvent, ProviderError>>, ProviderError> {
        let resp = self.complete(ctx, req).await?;
        let text = resp
            .content
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let events = vec![
            Ok(StreamEvent::TextDelta(text)),
            Ok(StreamEvent::Done(resp)),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}
