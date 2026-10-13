//! OpenAI-compatible provider — covers OpenAI proper and most local servers
//! (Ollama, llama.cpp, LM Studio, vLLM) that expose `/chat/completions`.
//!
//! Includes full tool-calling translation: the internal [`Block`] model maps to
//! OpenAI `tool_calls` / `role: "tool"` messages and back, so local models get
//! the same agentic loop as Anthropic.

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;
use truss_core::message::{Block, Message, Role};
use truss_core::provider::{
    CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason, StreamEvent,
    Usage,
};

pub struct OpenAi {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAi {
    pub fn new(model: String, api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".into()),
        }
    }

    fn request_body(&self, req: &CompletionRequest, stream: bool) -> Value {
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());

        let mut messages: Vec<Value> = Vec::new();
        let system = rendered_system(req);
        if let Some(sys) = system.as_deref() {
            if !sys.is_empty() {
                messages.push(json!({ "role": "system", "content": sys }));
            }
        }
        for m in &req.messages {
            messages.extend(message_to_json(m));
        }

        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "messages": messages,
        });
        if stream {
            body["stream"] = json!(true);
        }
        if let Some(temp) = req.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(level) = req.reasoning {
            body["reasoning_effort"] = json!(level.as_openai_str());
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect::<Vec<_>>());
        }
        body
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

fn rendered_system(req: &CompletionRequest) -> Option<String> {
    if !req.system_blocks.is_empty() {
        Some(
            req.system_blocks
                .iter()
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    } else {
        req.system.clone()
    }
}

#[derive(Debug, Clone, Default)]
struct ToolCallDelta {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone)]
struct OpenAiStreamState {
    model: String,
    text: String,
    tool_calls: Vec<ToolCallDelta>,
    stop_reason: StopReason,
    usage: Usage,
}

impl OpenAiStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn apply_chunk(&mut self, v: &Value) -> Result<Option<StreamEvent>, ProviderError> {
        if let Some(model) = v["model"].as_str() {
            self.model = model.to_string();
        }
        if let Some(usage) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        }

        let Some(choice) = v["choices"].as_array().and_then(|c| c.first()) else {
            return Ok(None);
        };

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop_reason = match reason {
                "stop" => StopReason::EndTurn,
                "length" => StopReason::MaxTokens,
                "tool_calls" => StopReason::ToolUse,
                other => StopReason::Other(other.to_string()),
            };
        }

        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                self.text.push_str(text);
                return Ok(Some(StreamEvent::TextDelta(text.to_string())));
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let idx = call["index"]
                    .as_u64()
                    .ok_or_else(|| ProviderError::Decode("missing tool call index".into()))?
                    as usize;
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(ToolCallDelta::default());
                }
                let slot = &mut self.tool_calls[idx];
                if let Some(id) = call["id"].as_str() {
                    slot.id = id.to_string();
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    slot.name.push_str(name);
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    slot.arguments.push_str(args);
                }
            }
        }

        Ok(None)
    }

    fn finish(self) -> CompletionResponse {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(Block::Text(self.text));
        }
        for call in self.tool_calls {
            if call.id.is_empty() && call.name.is_empty() {
                continue;
            }
            let input = serde_json::from_str::<Value>(&call.arguments)
                .unwrap_or_else(|_| json!({ "_raw": call.arguments }));
            content.push(Block::ToolUse {
                id: call.id,
                name: call.name,
                input,
            });
        }
        CompletionResponse {
            content,
            stop_reason: self.stop_reason,
            model: self.model,
            usage: self.usage,
        }
    }
}

/// Translate one internal message into one or more OpenAI wire messages.
fn message_to_json(m: &Message) -> Vec<Value> {
    match m.role {
        Role::Tool => m
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { id, content, .. } => Some(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": content,
                })),
                _ => None,
            })
            .collect(),
        Role::Assistant => {
            let text = m.text();
            let tool_calls: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        }
                    })),
                    _ => None,
                })
                .collect();
            let mut msg = json!({ "role": "assistant" });
            // OpenAI requires content to be present (null is allowed alongside tool_calls).
            msg["content"] = if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            vec![msg]
        }
        Role::User => vec![json!({ "role": "user", "content": m.text() })],
    }
}

#[async_trait]
impl Provider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    fn capabilities(&self, model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            vision: false,
            parallel_tool_calls: true,
            streaming: true,
            context_window: self.context_window(model),
            max_output_tokens: 16_384,
        }
    }

    fn context_window(&self, model: Option<&str>) -> u32 {
        truss_core::pricing::context_window(model.unwrap_or(&self.model))
    }

    async fn complete(
        &self,
        ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());
        let body = self.request_body(req, false);
        let rb = self
            .client
            .post(self.chat_completions_url())
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body);

        let resp = super::send(ctx, rb).await?;
        let v = super::read_json(resp).await?;

        let msg = &v["choices"][0]["message"];
        let mut content: Vec<Block> = Vec::new();
        if let Some(text) = msg["content"].as_str() {
            if !text.is_empty() {
                content.push(Block::Text(text.to_string()));
            }
        }
        if let Some(calls) = msg["tool_calls"].as_array() {
            for call in calls {
                let id = call["id"].as_str().unwrap_or_default().to_string();
                let name = call["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let args_str = call["function"]["arguments"].as_str().unwrap_or("{}");
                let input = serde_json::from_str::<Value>(args_str)
                    .unwrap_or_else(|_| json!({ "_raw": args_str }));
                content.push(Block::ToolUse { id, name, input });
            }
        }

        let stop_reason = match v["choices"][0]["finish_reason"].as_str() {
            Some("stop") => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some("tool_calls") => StopReason::ToolUse,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::Other("unknown".into()),
        };

        Ok(CompletionResponse {
            content,
            stop_reason,
            model: v["model"].as_str().unwrap_or(&model).to_string(),
            usage: Usage {
                input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            },
        })
    }

    async fn complete_stream<'a>(
        &'a self,
        ctx: &'a Ctx,
        req: &'a CompletionRequest,
    ) -> Result<futures::stream::BoxStream<'a, Result<StreamEvent, ProviderError>>, ProviderError>
    {
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());
        let body = self.request_body(req, true);
        let rb = self
            .client
            .post(self.chat_completions_url())
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body);

        let resp = super::ensure_success(super::send(ctx, rb).await?).await?;
        let stream = async_stream::try_stream! {
            let mut bytes = resp.bytes_stream();
            let mut buffer = String::new();
            let mut state = OpenAiStreamState::new(model);

            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|e| {
                    if e.is_timeout() {
                        ProviderError::Timeout
                    } else {
                        ProviderError::Transport(e.to_string())
                    }
                })?;
                let text = std::str::from_utf8(&chunk)
                    .map_err(|e| ProviderError::Decode(e.to_string()))?;
                for frame in super::push_sse_chunk(&mut buffer, text) {
                    if frame.data == "[DONE]" {
                        yield StreamEvent::Done(state.finish());
                        return;
                    }
                    let v: Value = serde_json::from_str(&frame.data)
                        .map_err(|e| ProviderError::Decode(e.to_string()))?;
                    if let Some(event) = state.apply_chunk(&v)? {
                        yield event;
                    }
                }
            }

            yield StreamEvent::Done(state.finish());
        };
        Ok(Box::pin(stream))
    }
}

/// Registry entry point: resolve auth and construct the provider. The OpenAI
/// surface also covers local servers (Ollama/vLLM/etc.) via `--base-url`, which
/// usually need no key — so a missing `OPENAI_API_KEY` is tolerated, not fatal.
/// Unlike Anthropic, there's no sensible default model, so one is required. See
/// [`crate::registry`].
pub(crate) fn build(
    cfg: &crate::registry::ProviderConfig,
) -> Result<std::sync::Arc<dyn Provider>, crate::registry::BuildError> {
    use crate::registry::BuildError;
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "no-key".into());
    let model = cfg
        .model
        .clone()
        .ok_or_else(|| BuildError::Config("--model is required for the openai provider".into()))?;
    Ok(std::sync::Arc::new(OpenAi::new(
        model,
        api_key,
        cfg.base_url.clone(),
    )))
}
