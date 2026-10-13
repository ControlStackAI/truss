//! Anthropic Messages API provider — full text + tool-use support.

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;
use truss_core::message::{Block, Role};
use truss_core::provider::{
    CacheControl, CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason,
    StreamEvent, ToolDef, Usage,
};

const ANTHROPIC_MEMORY_TOOL_TYPE: &str = "memory_20250818";
const ANTHROPIC_TEXT_EDITOR_TOOL_TYPE: &str = "text_editor_20250728";
const ANTHROPIC_TEXT_EDITOR_TOOL_NAME: &str = "str_replace_based_edit_tool";
const ANTHROPIC_BASH_TOOL_TYPE: &str = "bash_20250124";

pub struct Anthropic {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl Anthropic {
    pub fn new(model: String, api_key: String, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
        }
    }

    /// Default model when the caller doesn't pin one.
    pub const DEFAULT_MODEL: &'static str = "claude-opus-4-8";

    fn request_body(&self, req: &CompletionRequest, stream: bool) -> Value {
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| {
                json!({
                    "role": role_str(m.role),
                    "content": m.content.iter().map(block_to_json).collect::<Vec<_>>(),
                })
            })
            .collect();

        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "messages": messages,
        });
        if stream {
            body["stream"] = json!(true);
        }
        if !req.system_blocks.is_empty() {
            body["system"] = json!(req
                .system_blocks
                .iter()
                .map(|block| {
                    let mut v = json!({ "type": "text", "text": block.text });
                    if matches!(block.cache_control, Some(CacheControl::Ephemeral)) {
                        v["cache_control"] = json!({ "type": "ephemeral" });
                    }
                    v
                })
                .collect::<Vec<_>>());
        } else if let Some(sys) = &req.system {
            body["system"] = json!(sys);
        }
        // Extended thinking. Budget must be < max_tokens; Anthropic also requires
        // temperature to be unset when thinking is enabled, so we omit it here.
        match req.reasoning {
            Some(level) => {
                let budget = level
                    .anthropic_budget_tokens()
                    .min(req.max_tokens.saturating_sub(512));
                if budget >= 1024 {
                    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
                } else {
                    // max_tokens is too small to fit a thinking budget — don't
                    // silently pretend we enabled it.
                    tracing::warn!(
                        max_tokens = req.max_tokens,
                        "reasoning requested but max_tokens too small for a thinking budget; \
                         sending the request without extended thinking"
                    );
                    if let Some(temp) = req.temperature {
                        body["temperature"] = json!(temp);
                    }
                }
            }
            None => {
                if let Some(temp) = req.temperature {
                    body["temperature"] = json!(temp);
                }
            }
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(anthropic_tool_defs(&req.tools));
        }
        body
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }
}

fn anthropic_tool_defs(tools: &[ToolDef]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut emitted_text_editor = false;

    for tool in tools {
        match tool.name.as_str() {
            "memory" if tool.input_schema["type"] == ANTHROPIC_MEMORY_TOOL_TYPE => {
                out.push(json!({
                    "type": ANTHROPIC_MEMORY_TOOL_TYPE,
                    "name": tool.name,
                }));
            }
            "read_file" | "write_file" | "list_dir" => {
                if !emitted_text_editor {
                    emitted_text_editor = true;
                    out.push(json!({
                        "type": ANTHROPIC_TEXT_EDITOR_TOOL_TYPE,
                        "name": ANTHROPIC_TEXT_EDITOR_TOOL_NAME,
                    }));
                }
            }
            "bash" => {
                out.push(json!({
                    "type": ANTHROPIC_BASH_TOOL_TYPE,
                    "name": "bash",
                }));
            }
            _ => {
                out.push(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                }));
            }
        }
    }

    out
}

#[derive(Debug, Clone)]
enum AnthropicContentBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
}

#[derive(Debug, Clone)]
struct AnthropicStreamState {
    model: String,
    blocks: std::collections::BTreeMap<usize, AnthropicContentBlock>,
    stop_reason: StopReason,
    usage: Usage,
}

impl AnthropicStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            blocks: std::collections::BTreeMap::new(),
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn apply_event(
        &mut self,
        event: Option<&str>,
        data: &str,
    ) -> Result<Option<StreamEvent>, ProviderError> {
        let v: Value =
            serde_json::from_str(data).map_err(|e| ProviderError::Decode(e.to_string()))?;
        match event.or_else(|| v["type"].as_str()) {
            Some("message_start") => {
                if let Some(model) = v["message"]["model"].as_str() {
                    self.model = model.to_string();
                }
                self.usage.input_tokens =
                    v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
            }
            Some("content_block_start") => {
                let index = v["index"]
                    .as_u64()
                    .ok_or_else(|| ProviderError::Decode("missing content block index".into()))?
                    as usize;
                let block = &v["content_block"];
                match block["type"].as_str() {
                    Some("text") => {
                        self.blocks.insert(
                            index,
                            AnthropicContentBlock::Text(
                                block["text"].as_str().unwrap_or_default().to_string(),
                            ),
                        );
                    }
                    Some("tool_use") => {
                        self.blocks.insert(
                            index,
                            AnthropicContentBlock::ToolUse {
                                id: block["id"].as_str().unwrap_or_default().to_string(),
                                name: block["name"].as_str().unwrap_or_default().to_string(),
                                partial_json: String::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let index = v["index"]
                    .as_u64()
                    .ok_or_else(|| ProviderError::Decode("missing content block index".into()))?
                    as usize;
                let delta = &v["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        if let Some(AnthropicContentBlock::Text(acc)) = self.blocks.get_mut(&index)
                        {
                            acc.push_str(text);
                        } else {
                            self.blocks
                                .insert(index, AnthropicContentBlock::Text(text.to_string()));
                        }
                        if !text.is_empty() {
                            return Ok(Some(StreamEvent::TextDelta(text.to_string())));
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = delta["partial_json"].as_str().unwrap_or_default();
                        if let Some(AnthropicContentBlock::ToolUse { partial_json, .. }) =
                            self.blocks.get_mut(&index)
                        {
                            partial_json.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = match reason {
                        "end_turn" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        other => StopReason::Other(other.to_string()),
                    };
                }
                if let Some(out) = v["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = out;
                }
            }
            Some("message_stop") => return Ok(Some(StreamEvent::Done(self.clone().finish()))),
            _ => {}
        }

        Ok(None)
    }

    fn finish(self) -> CompletionResponse {
        let content = self
            .blocks
            .into_values()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text(text) if !text.is_empty() => Some(Block::Text(text)),
                AnthropicContentBlock::ToolUse {
                    id,
                    name,
                    partial_json,
                } => {
                    let input = serde_json::from_str::<Value>(&partial_json)
                        .unwrap_or_else(|_| json!({ "_raw": partial_json }));
                    Some(Block::ToolUse { id, name, input })
                }
                _ => None,
            })
            .collect();
        CompletionResponse {
            content,
            stop_reason: self.stop_reason,
            model: self.model,
            usage: self.usage,
        }
    }
}

/// Anthropic has no distinct "tool" role — tool results ride in a user turn.
fn role_str(r: Role) -> &'static str {
    match r {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
    }
}

fn block_to_json(b: &Block) -> Value {
    match b {
        Block::Text(t) => json!({ "type": "text", "text": t }),
        Block::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        Block::ToolResult {
            id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": content,
            "is_error": is_error
        }),
    }
}

fn parse_block(v: &Value) -> Option<Block> {
    match v["type"].as_str()? {
        "text" => Some(Block::Text(v["text"].as_str()?.to_string())),
        "tool_use" => Some(Block::ToolUse {
            id: v["id"].as_str()?.to_string(),
            name: v["name"].as_str()?.to_string(),
            input: v["input"].clone(),
        }),
        _ => None, // thinking blocks etc. are ignored for now
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_model(&self) -> &str {
        &self.model
    }

    fn capabilities(&self, model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            vision: true,
            parallel_tool_calls: true,
            streaming: true,
            context_window: self.context_window(model),
            max_output_tokens: 8_192,
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
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        let resp = super::send(ctx, rb).await?;
        let v = super::read_json(resp).await?;

        let content = v["content"]
            .as_array()
            .ok_or_else(|| ProviderError::Decode("missing content array".into()))?
            .iter()
            .filter_map(parse_block)
            .collect();

        let stop_reason = match v["stop_reason"].as_str() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::Other("unknown".into()),
        };

        Ok(CompletionResponse {
            content,
            stop_reason,
            model: v["model"].as_str().unwrap_or(&model).to_string(),
            usage: Usage {
                input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
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
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);

        let resp = super::ensure_success(super::send(ctx, rb).await?).await?;
        let stream = async_stream::try_stream! {
            let mut bytes = resp.bytes_stream();
            let mut buffer = String::new();
            let mut state = AnthropicStreamState::new(model);

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
                    if let Some(event) = state.apply_event(frame.event.as_deref(), &frame.data)? {
                        yield event;
                        if matches!(frame.event.as_deref(), Some("message_stop")) {
                            return;
                        }
                    }
                }
            }

            yield StreamEvent::Done(state.finish());
        };
        Ok(Box::pin(stream))
    }
}

/// Registry entry point: resolve auth (here, an API key from the environment) and
/// construct the provider. A subscription/OAuth variant would slot in here without
/// touching the caller. See [`crate::registry`].
pub(crate) fn build(
    cfg: &crate::registry::ProviderConfig,
) -> Result<std::sync::Arc<dyn Provider>, crate::registry::BuildError> {
    use crate::registry::BuildError;
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| BuildError::Auth("ANTHROPIC_API_KEY is not set".into()))?;
    let model = cfg
        .model
        .clone()
        .unwrap_or_else(|| Anthropic::DEFAULT_MODEL.to_string());
    Ok(std::sync::Arc::new(Anthropic::new(
        model,
        api_key,
        cfg.base_url.clone(),
    )))
}
