//! Provider wire-translation tests — no network, via a mock HTTP server.
//!
//! Covers the request/response translation and error classification for the
//! Anthropic and OpenAI backends: text, tool-use, and HTTP error mapping. These
//! are the parts that talk to the outside world, so they're the most important to
//! pin down. Only compiled when both HTTP providers are enabled.
#![cfg(all(feature = "provider-anthropic", feature = "provider-openai"))]

use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use truss_core::ctx::Ctx;
use truss_core::error::ProviderError;
use truss_core::message::{Block, Message};
use truss_core::provider::{
    CompletionRequest, Provider, ReasoningEffort, StopReason, StreamEvent, SystemBlock, ToolDef,
};
use truss_providers::{Anthropic, OpenAi};

use futures::StreamExt;

fn req() -> CompletionRequest {
    CompletionRequest::new(vec![Message::user("hi")])
}

fn sse_json(v: serde_json::Value) -> String {
    format!("data: {v}\n\n")
}

fn sse_event(event: &str, v: serde_json::Value) -> String {
    format!("event: {event}\ndata: {v}\n\n")
}

// ── Anthropic ─────────────────────────────────────────────────────────────

#[test]
fn provider_context_windows_use_known_models_and_fallbacks() {
    let anthropic = Anthropic::new("claude-opus-4-8".into(), "k".into(), None);
    assert_eq!(anthropic.context_window(None), 200_000);

    let openai = OpenAi::new("gpt-5.5-codex".into(), "k".into(), None);
    assert_eq!(openai.context_window(None), 256_000);
    assert_eq!(
        openai.context_window(Some("unknown-local-model")),
        truss_core::pricing::DEFAULT_CONTEXT_WINDOW
    );
}

#[tokio::test]
async fn anthropic_parses_text_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "k")) // proves we send auth
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "hello world"}],
            "stop_reason": "end_turn",
            "model": "claude-x",
            "usage": {"input_tokens": 12, "output_tokens": 3}
        })))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let resp = p.complete(&Ctx::new(), &req()).await.unwrap();

    assert!(matches!(&resp.content[0], Block::Text(t) if t == "hello world"));
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.usage.input_tokens, 12);
    assert_eq!(resp.usage.output_tokens, 3);
}

#[tokio::test]
async fn anthropic_parses_tool_use() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "tool_use", "id": "t1", "name": "read_file",
                         "input": {"path": "x"}}],
            "stop_reason": "tool_use",
            "model": "claude-x",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let resp = p.complete(&Ctx::new(), &req()).await.unwrap();

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    match &resp.content[0] {
        Block::ToolUse { id, name, input } => {
            assert_eq!(id, "t1");
            assert_eq!(name, "read_file");
            assert_eq!(input["path"], "x");
        }
        other => panic!("expected tool_use, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_serializes_native_memory_tool_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"type\":\"memory_20250818\""))
        .and(body_string_contains("\"name\":\"memory\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "model": "claude-x",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let mut request = req();
    request.tools.push(ToolDef {
        name: "memory".into(),
        description: "memory".into(),
        input_schema: json!({ "type": "memory_20250818" }),
    });

    let resp = p.complete(&Ctx::new(), &request).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn anthropic_serializes_typed_file_and_shell_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"type\":\"text_editor_20250728\""))
        .and(body_string_contains(
            "\"name\":\"str_replace_based_edit_tool\"",
        ))
        .and(body_string_contains("\"type\":\"bash_20250124\""))
        .and(body_string_contains("\"name\":\"bash\""))
        .and(body_string_contains("\"type\":\"memory_20250818\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "model": "claude-x",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let mut request = req();
    request.tools = vec![
        ToolDef {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "write_file".into(),
            description: "write".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "list_dir".into(),
            description: "list".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "bash".into(),
            description: "shell".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "memory".into(),
            description: "memory".into(),
            input_schema: json!({ "type": "memory_20250818" }),
        },
    ];

    let resp = p.complete(&Ctx::new(), &request).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn anthropic_serializes_system_cache_control_breakpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains(
            "\"cache_control\":{\"type\":\"ephemeral\"}",
        ))
        .and(body_string_contains("\"text\":\"L1 principal\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "model": "claude-x",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let mut request = req();
    request.system_blocks = vec![
        SystemBlock::new("L0 core"),
        SystemBlock::new("L0 self"),
        SystemBlock::cached("L1 principal"),
    ];

    let resp = p.complete(&Ctx::new(), &request).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn anthropic_streams_text_and_final_response() {
    let server = MockServer::start().await;
    let body = [
        sse_event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "model": "claude-x",
                    "usage": {"input_tokens": 9}
                }
            }),
        ),
        sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ),
        sse_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "hel"}
            }),
        ),
        sse_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "lo"}
            }),
        ),
        sse_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2}
            }),
        ),
        sse_event("message_stop", json!({"type": "message_stop"})),
    ]
    .join("");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"stream\":true"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let ctx = Ctx::new();
    let request = req();
    let mut stream = p.complete_stream(&ctx, &request).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }

    assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "hel"));
    assert!(matches!(&events[1], StreamEvent::TextDelta(t) if t == "lo"));
    match events.last().unwrap() {
        StreamEvent::Done(resp) => {
            assert!(matches!(&resp.content[0], Block::Text(t) if t == "hello"));
            assert_eq!(resp.stop_reason, StopReason::EndTurn);
            assert_eq!(resp.usage.input_tokens, 9);
            assert_eq!(resp.usage.output_tokens, 2);
        }
        other => panic!("expected final Done, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_streams_tool_use() {
    let server = MockServer::start().await;
    let body = [
        sse_event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {"model": "claude-x", "usage": {"input_tokens": 1}}
            }),
        ),
        sse_event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "t1", "name": "ping", "input": {}}
            }),
        ),
        sse_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"a\""}
            }),
        ),
        sse_event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": ":1}"}
            }),
        ),
        sse_event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 1}
            }),
        ),
        sse_event("message_stop", json!({"type": "message_stop"})),
    ]
    .join("");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let ctx = Ctx::new();
    let request = req();
    let mut stream = p.complete_stream(&ctx, &request).await.unwrap();
    let mut done = None;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Done(resp) = event.unwrap() {
            done = Some(resp);
        }
    }
    let resp = done.expect("stream should finish");
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    match &resp.content[0] {
        Block::ToolUse { id, name, input } => {
            assert_eq!(id, "t1");
            assert_eq!(name, "ping");
            assert_eq!(input["a"], 1);
        }
        other => panic!("expected tool_use, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_maps_429_to_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server)
        .await;

    let p = Anthropic::new("m".into(), "k".into(), Some(server.uri()));
    let err = p.complete(&Ctx::new(), &req()).await.unwrap_err();
    match err {
        ProviderError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, Some(7));
            assert!(ProviderError::RateLimited { retry_after_secs }.is_retryable());
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn anthropic_maps_500_to_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let p = Anthropic::new("m".into(), "k".into(), Some(server.uri()));
    let err = p.complete(&Ctx::new(), &req()).await.unwrap_err();
    match err {
        ProviderError::Status { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("boom"));
            assert!(ProviderError::Status { status, body }.is_retryable()); // 5xx retryable
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

// ── OpenAI ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn openai_parses_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "hey"}, "finish_reason": "stop"}],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 8, "completion_tokens": 4}
        })))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let resp = p.complete(&Ctx::new(), &req()).await.unwrap();

    assert!(matches!(&resp.content[0], Block::Text(t) if t == "hey"));
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.usage.input_tokens, 8);
    assert_eq!(resp.usage.output_tokens, 4);
}

#[tokio::test]
async fn openai_translates_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": {"name": "ping", "arguments": "{\"a\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let resp = p.complete(&Ctx::new(), &req()).await.unwrap();

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    match resp
        .content
        .iter()
        .find(|b| matches!(b, Block::ToolUse { .. }))
    {
        Some(Block::ToolUse { id, name, input }) => {
            assert_eq!(id, "c1");
            assert_eq!(name, "ping");
            assert_eq!(input["a"], 1);
        }
        _ => panic!("expected a tool_use block"),
    }
}

#[tokio::test]
async fn openai_serializes_file_and_shell_tools_as_generic_functions() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"type\":\"function\""))
        .and(body_string_contains("\"name\":\"read_file\""))
        .and(body_string_contains("\"name\":\"write_file\""))
        .and(body_string_contains("\"name\":\"list_dir\""))
        .and(body_string_contains("\"name\":\"bash\""))
        .and(body_string_contains("\"parameters\":{\"type\":\"object\"}"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let mut request = req();
    request.tools = vec![
        ToolDef {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "write_file".into(),
            description: "write".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "list_dir".into(),
            description: "list".into(),
            input_schema: json!({ "type": "object" }),
        },
        ToolDef {
            name: "bash".into(),
            description: "shell".into(),
            input_schema: json!({ "type": "object" }),
        },
    ];

    let resp = p.complete(&Ctx::new(), &request).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn openai_streams_text_and_final_response() {
    let server = MockServer::start().await;
    let body = [
        sse_json(json!({
            "choices": [{"delta": {"content": "hel"}, "finish_reason": null}],
            "model": "gpt-x"
        })),
        sse_json(json!({
            "choices": [{"delta": {"content": "lo"}, "finish_reason": null}],
            "model": "gpt-x"
        })),
        sse_json(json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        })),
        "data: [DONE]\n\n".to_string(),
    ]
    .join("");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"stream\":true"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let ctx = Ctx::new();
    let request = req();
    let mut stream = p.complete_stream(&ctx, &request).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }

    assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "hel"));
    assert!(matches!(&events[1], StreamEvent::TextDelta(t) if t == "lo"));
    match events.last().unwrap() {
        StreamEvent::Done(resp) => {
            assert!(matches!(&resp.content[0], Block::Text(t) if t == "hello"));
            assert_eq!(resp.stop_reason, StopReason::EndTurn);
            assert_eq!(resp.usage.input_tokens, 4);
            assert_eq!(resp.usage.output_tokens, 2);
        }
        other => panic!("expected final Done, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_streams_tool_calls() {
    let server = MockServer::start().await;
    let body = [
        sse_json(json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "ping", "arguments": "{\"a\""}
                    }]
                },
                "finish_reason": null
            }],
            "model": "gpt-x"
        })),
        sse_json(json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": ":1}"}
                    }]
                },
                "finish_reason": null
            }],
            "model": "gpt-x"
        })),
        sse_json(json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })),
        "data: [DONE]\n\n".to_string(),
    ]
    .join("");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let ctx = Ctx::new();
    let request = req();
    let mut stream = p.complete_stream(&ctx, &request).await.unwrap();
    let mut done = None;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Done(resp) = event.unwrap() {
            done = Some(resp);
        }
    }
    let resp = done.expect("stream should finish");
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    match &resp.content[0] {
        Block::ToolUse { id, name, input } => {
            assert_eq!(id, "c1");
            assert_eq!(name, "ping");
            assert_eq!(input["a"], 1);
        }
        other => panic!("expected tool_use, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_maps_500_to_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&server)
        .await;

    let p = OpenAi::new("m".into(), "k".into(), Some(server.uri()));
    let err = p.complete(&Ctx::new(), &req()).await.unwrap_err();
    assert!(matches!(err, ProviderError::Status { status: 503, .. }));
}

// ── Reasoning / thinking level ─────────────────────────────────────────────

#[tokio::test]
async fn openai_sends_reasoning_effort() {
    let server = MockServer::start().await;
    // The mock only matches if the body carries reasoning_effort=high, so a
    // successful call proves we sent it.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"reasoning_effort\":\"high\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let mut r = req();
    r.reasoning = Some(ReasoningEffort::High);
    let resp = p.complete(&Ctx::new(), &r).await.unwrap();
    assert!(matches!(&resp.content[0], Block::Text(t) if t == "ok"));
}

#[tokio::test]
async fn anthropic_sends_thinking_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"thinking\""))
        .and(body_string_contains("\"budget_tokens\":8192"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "model": "claude-x",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = Anthropic::new("claude-x".into(), "k".into(), Some(server.uri()));
    let mut r = req();
    r.max_tokens = 20_000; // leave room so the budget isn't clamped
    r.reasoning = Some(ReasoningEffort::Medium);
    let resp = p.complete(&Ctx::new(), &r).await.unwrap();
    assert!(matches!(&resp.content[0], Block::Text(t) if t == "ok"));
}

#[tokio::test]
async fn unsupported_reasoning_is_a_clean_fatal_error() {
    // A model that doesn't support the requested level returns 400. We must
    // surface it as a typed, NON-retryable error (not loop forever).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("unsupported value: 'reasoning_effort' for this model"),
        )
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-4o".into(), "k".into(), Some(server.uri()));
    let mut r = req();
    r.reasoning = Some(ReasoningEffort::High);
    let err = p.complete(&Ctx::new(), &r).await.unwrap_err();
    match err {
        ProviderError::Status { status, .. } => {
            assert_eq!(status, 400);
            assert!(
                !ProviderError::Status {
                    status,
                    body: String::new()
                }
                .is_retryable(),
                "a 4xx must be fatal, not retried"
            );
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_sends_temperature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"temperature\":0.3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "model": "gpt-x",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&server)
        .await;

    let p = OpenAi::new("gpt-x".into(), "k".into(), Some(server.uri()));
    let mut r = req();
    r.temperature = Some(0.3);
    let resp = p.complete(&Ctx::new(), &r).await.unwrap();
    assert!(matches!(&resp.content[0], Block::Text(t) if t == "ok"));
}
