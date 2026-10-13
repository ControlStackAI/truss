//! OpenAI Codex **subscription** provider (ADR 0003, Tier 1).
//!
//! In-process OAuth-replay: drives the model on a ChatGPT subscription using the
//! credentials the `codex` CLI already stored (`~/.codex/auth.json`, via
//! [`crate::codex_auth`]). Because it's a normal [`Provider`] — one request in,
//! one response out — it behaves *just like the API path* and runs Truss's full
//! durable, signed, N-reviewer-verified loop. Ideal as a no-API-cost dev/test
//! driver.
//!
//! Wire format is the ChatGPT-backend **Responses** API
//! (`POST https://chatgpt.com/backend-api/codex/responses`), taken verbatim from
//! `openai/codex` (`codex-rs/codex-api/src/...`): `store:false`, `stream:true`,
//! `input` items (`message`/`function_call`/`function_call_output`), function tools
//! flattened (`{type:"function", name, description, strict, parameters}`), and SSE
//! events keyed on a JSON `type` (`response.output_text.delta`,
//! `response.output_item.done`, `response.completed`, `response.failed`). Tool calls
//! arrive **whole** in `response.output_item.done` (the `function_call_arguments.*`
//! deltas are not used).
//!
//! NOTE: this is a reverse-engineered, ToS-grey path (see `docs/auth-strategy.md`).
//! It's behind the `provider-openai-codex` feature, off by default, and the live
//! request headers may need tuning against the real backend — needs a smoke test
//! with a real login before it's trusted in anger.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;
use truss_core::message::{Block, Role};
use truss_core::provider::{
    CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason, StreamEvent,
    Usage,
};

use crate::codex_auth::CodexAuth;

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// `codex` reports its own crate version in `originator`/`version`; we present a
/// recent value. Tune if the backend gets stricter about client identity.
const CODEX_VERSION: &str = "0.140.0";

pub struct OpenAiCodex {
    client: reqwest::Client,
    auth: Arc<CodexAuth>,
    model: String,
    base_url: String,
}

impl OpenAiCodex {
    /// Default model slug when the caller doesn't pin one.
    pub const DEFAULT_MODEL: &'static str = "gpt-5.5-codex";

    pub fn new(model: String, auth: Arc<CodexAuth>, base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into()),
        }
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    /// Build the Responses-API request body for `req`.
    fn request_body(&self, req: &CompletionRequest) -> Value {
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());

        let mut input: Vec<Value> = Vec::new();
        for m in &req.messages {
            match m.role {
                Role::User => {
                    let content: Vec<Value> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text(t) => Some(json!({ "type": "input_text", "text": t })),
                            _ => None,
                        })
                        .collect();
                    if !content.is_empty() {
                        input
                            .push(json!({ "type": "message", "role": "user", "content": content }));
                    }
                }
                Role::Assistant => {
                    let text = m.text();
                    if !text.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": text }],
                        }));
                    }
                    for b in &m.content {
                        if let Block::ToolUse {
                            id,
                            name,
                            input: args,
                        } = b
                        {
                            input.push(json!({
                                "type": "function_call",
                                "name": name,
                                "arguments": args.to_string(),
                                "call_id": id,
                            }));
                        }
                    }
                }
                Role::Tool => {
                    for b in &m.content {
                        if let Block::ToolResult { id, content, .. } = b {
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": id,
                                "output": content,
                            }));
                        }
                    }
                }
            }
        }

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "strict": false,
                    "parameters": t.input_schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": model,
            "input": input,
            "tools": tools,
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true,
        });
        let system = rendered_system(req);
        if let Some(sys) = system.as_deref() {
            if !sys.is_empty() {
                body["instructions"] = json!(sys);
            }
        }
        // `reasoning` is serialized even when absent (matches codex), and reasoning
        // turns on encrypted-content include.
        if let Some(level) = req.reasoning {
            body["reasoning"] = json!({ "effort": level.as_openai_str(), "summary": "auto" });
            body["include"] = json!(["reasoning.encrypted_content"]);
        } else {
            body["reasoning"] = Value::Null;
            body["include"] = json!([]);
        }
        body
    }

    /// One POST to the Responses endpoint with current creds.
    async fn send_once(&self, ctx: &Ctx, body: &Value) -> Result<reqwest::Response, ProviderError> {
        let (token, account) = self.auth.current().await;
        let mut rb = self
            .client
            .post(self.responses_url())
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("originator", "codex_cli_rs")
            .header(
                "user-agent",
                format!("codex_cli_rs/{CODEX_VERSION} (truss)"),
            )
            .header("version", CODEX_VERSION);
        if let Some(acc) = account {
            rb = rb.header("ChatGPT-Account-ID", acc);
        }
        super::send(ctx, rb.json(body)).await
    }

    /// POST with a one-shot reactive refresh on 401 (the access token expired).
    async fn post_responses(
        &self,
        ctx: &Ctx,
        body: &Value,
    ) -> Result<reqwest::Response, ProviderError> {
        let resp = self.send_once(ctx, body).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.auth.refresh(ctx).await?;
            let resp = self.send_once(ctx, body).await?;
            return super::ensure_success(resp).await;
        }
        super::ensure_success(resp).await
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

/// Accumulates the SSE stream into a final response.
struct CodexStreamState {
    model: String,
    text: String,
    tool_calls: Vec<Block>,
    usage: Usage,
    stop_reason: StopReason,
}

impl CodexStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            text: String::new(),
            tool_calls: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
        }
    }

    /// Apply one parsed SSE event; returns a `TextDelta` to forward, if any.
    fn apply(&mut self, v: &Value) -> Result<Option<StreamEvent>, ProviderError> {
        match v["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(d) = v["delta"].as_str() {
                    if !d.is_empty() {
                        self.text.push_str(d);
                        return Ok(Some(StreamEvent::TextDelta(d.to_string())));
                    }
                }
            }
            // Tool calls arrive finalized here (not via arguments deltas).
            "response.output_item.done" => {
                let item = &v["item"];
                if item["type"].as_str() == Some("function_call") {
                    let id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let args_str = item["arguments"].as_str().unwrap_or("{}");
                    let input = serde_json::from_str::<Value>(args_str)
                        .unwrap_or_else(|_| json!({ "_raw": args_str }));
                    self.tool_calls.push(Block::ToolUse { id, name, input });
                    self.stop_reason = StopReason::ToolUse;
                }
            }
            "response.completed" => {
                let u = &v["response"]["usage"];
                self.usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                self.usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "response.failed" => {
                let msg = v["response"]["error"]["message"]
                    .as_str()
                    .unwrap_or("response.failed")
                    .to_string();
                return Err(ProviderError::Status {
                    status: 502,
                    body: msg,
                });
            }
            _ => {}
        }
        Ok(None)
    }

    fn finish(self) -> CompletionResponse {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(Block::Text(self.text));
        }
        content.extend(self.tool_calls);
        CompletionResponse {
            content,
            stop_reason: self.stop_reason,
            model: self.model,
            usage: self.usage,
        }
    }
}

#[async_trait]
impl Provider for OpenAiCodex {
    fn name(&self) -> &str {
        "openai-codex"
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
        // The Responses backend is stream-only (store:false, stream:true); drain
        // the SSE and aggregate.
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());
        let body = self.request_body(req);
        let resp = self.post_responses(ctx, &body).await?;

        let mut bytes = resp.bytes_stream();
        let mut buffer = String::new();
        let mut state = CodexStreamState::new(model);
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Transport(e.to_string())
                }
            })?;
            let text =
                std::str::from_utf8(&chunk).map_err(|e| ProviderError::Decode(e.to_string()))?;
            for frame in super::push_sse_chunk(&mut buffer, text) {
                let v: Value = serde_json::from_str(&frame.data)
                    .map_err(|e| ProviderError::Decode(e.to_string()))?;
                state.apply(&v)?;
            }
        }
        Ok(state.finish())
    }

    async fn complete_stream<'a>(
        &'a self,
        ctx: &'a Ctx,
        req: &'a CompletionRequest,
    ) -> Result<futures::stream::BoxStream<'a, Result<StreamEvent, ProviderError>>, ProviderError>
    {
        let model = req.model.clone().unwrap_or_else(|| self.model.clone());
        let body = self.request_body(req);
        let resp = self.post_responses(ctx, &body).await?;

        let stream = async_stream::try_stream! {
            let mut bytes = resp.bytes_stream();
            let mut buffer = String::new();
            let mut state = CodexStreamState::new(model);

            while let Some(chunk) = bytes.next().await {
                let chunk = chunk.map_err(|e| {
                    if e.is_timeout() { ProviderError::Timeout } else { ProviderError::Transport(e.to_string()) }
                })?;
                let text = std::str::from_utf8(&chunk)
                    .map_err(|e| ProviderError::Decode(e.to_string()))?;
                for frame in super::push_sse_chunk(&mut buffer, text) {
                    let v: Value = serde_json::from_str(&frame.data)
                        .map_err(|e| ProviderError::Decode(e.to_string()))?;
                    if let Some(event) = state.apply(&v)? {
                        yield event;
                    }
                }
            }
            yield StreamEvent::Done(state.finish());
        };
        Ok(Box::pin(stream))
    }
}

/// Registry entry point: load the Codex login (`~/.codex/auth.json`) and build the
/// subscription provider. Errors if no ChatGPT login is present.
pub(crate) fn build(
    cfg: &crate::registry::ProviderConfig,
) -> Result<Arc<dyn Provider>, crate::registry::BuildError> {
    use crate::registry::BuildError;
    let auth = CodexAuth::load().map_err(BuildError::Auth)?;
    let model = cfg
        .model
        .clone()
        .unwrap_or_else(|| OpenAiCodex::DEFAULT_MODEL.to_string());
    Ok(Arc::new(OpenAiCodex::new(
        model,
        Arc::new(auth),
        cfg.base_url.clone(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use truss_core::message::Message;
    use truss_core::provider::{ReasoningEffort, ToolDef};

    fn provider() -> OpenAiCodex {
        let auth = CodexAuth::from_value(json!({
            "tokens": { "access_token": "tok", "refresh_token": "rt", "account_id": "acc" }
        }))
        .unwrap();
        OpenAiCodex::new("gpt-5.5-codex".into(), Arc::new(auth), None)
    }

    #[test]
    fn context_window_uses_known_model_and_unknown_fallback() {
        let p = provider();

        assert_eq!(p.context_window(None), 256_000);
        assert_eq!(
            p.context_window(Some("unknown-codex-model")),
            truss_core::pricing::DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn request_body_uses_responses_shape() {
        let p = provider();
        let mut req = CompletionRequest::new(vec![Message::user("hello")]);
        req.system = Some("be terse".into());
        req.tools = vec![ToolDef {
            name: "read_file".into(),
            description: "reads".into(),
            input_schema: json!({ "type": "object" }),
        }];
        req.reasoning = Some(ReasoningEffort::High);

        let b = p.request_body(&req);
        assert_eq!(b["stream"], json!(true));
        assert_eq!(b["store"], json!(false)); // never persist on the ChatGPT backend
        assert_eq!(b["instructions"], json!("be terse"));
        assert_eq!(b["input"][0]["type"], json!("message"));
        assert_eq!(b["input"][0]["role"], json!("user"));
        assert_eq!(b["input"][0]["content"][0]["type"], json!("input_text"));
        assert_eq!(b["input"][0]["content"][0]["text"], json!("hello"));
        assert_eq!(b["tools"][0]["type"], json!("function"));
        assert_eq!(b["tools"][0]["name"], json!("read_file"));
        assert_eq!(b["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn tool_use_and_result_map_to_function_items() {
        let p = provider();
        let convo = vec![
            Message {
                role: Role::Assistant,
                content: vec![Block::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: json!({ "path": "x" }),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![Block::ToolResult {
                    id: "call_1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            },
        ];
        let b = p.request_body(&CompletionRequest::new(convo));
        let items = b["input"].as_array().unwrap();
        assert_eq!(items[0]["type"], json!("function_call"));
        assert_eq!(items[0]["call_id"], json!("call_1"));
        assert_eq!(items[0]["arguments"], json!("{\"path\":\"x\"}"));
        assert_eq!(items[1]["type"], json!("function_call_output"));
        assert_eq!(items[1]["call_id"], json!("call_1"));
        assert_eq!(items[1]["output"], json!("contents"));
    }

    #[test]
    fn sse_stream_assembles_text_tool_and_usage() {
        let mut s = CodexStreamState::new("gpt-5.5-codex".into());
        s.apply(&json!({ "type": "response.output_text.delta", "delta": "Hel" }))
            .unwrap();
        s.apply(&json!({ "type": "response.output_text.delta", "delta": "lo" }))
            .unwrap();
        s.apply(&json!({
            "type": "response.output_item.done",
            "item": { "type": "function_call", "call_id": "c1", "name": "read_file", "arguments": "{\"path\":\"a\"}" }
        }))
        .unwrap();
        s.apply(&json!({
            "type": "response.completed",
            "response": { "usage": { "input_tokens": 11, "output_tokens": 7 } }
        }))
        .unwrap();

        let r = s.finish();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert!(matches!(&r.content[0], Block::Text(t) if t == "Hello"));
        match &r.content[1] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], json!("a"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert_eq!(r.usage.input_tokens, 11);
        assert_eq!(r.usage.output_tokens, 7);
    }

    #[test]
    fn response_failed_is_an_error() {
        let mut s = CodexStreamState::new("m".into());
        let err = s
            .apply(&json!({
                "type": "response.failed",
                "response": { "error": { "message": "boom" } }
            }))
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");
    }
}
