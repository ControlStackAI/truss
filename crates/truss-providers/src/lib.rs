//! Concrete backends behind the [`registry`] plugin boundary.
//!
//! Callers go through [`registry::build`] — they never name a concrete provider
//! or resolve credentials. Add a new provider by implementing
//! [`truss_core::provider::Provider`] in a sibling module, giving it a
//! `pub(crate) fn build(&ProviderConfig)`, and registering it in [`registry`].

pub mod registry;

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "provider-openai-codex")]
pub mod codex_auth;
#[cfg(feature = "provider-openai")]
pub mod embedding;
#[cfg(feature = "provider-openai")]
pub mod openai;
#[cfg(feature = "provider-openai-codex")]
pub mod openai_codex;

#[cfg(feature = "provider-anthropic")]
pub use anthropic::Anthropic;
#[cfg(feature = "provider-openai")]
pub use embedding::OpenAiEmbedder;
#[cfg(feature = "provider-openai")]
pub use openai::OpenAi;
#[cfg(feature = "provider-openai-codex")]
pub use openai_codex::OpenAiCodex;

pub use registry::{available, build, BuildError, ProviderConfig};

// Shared HTTP helpers — compiled only when an HTTP provider is enabled.
#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
use serde_json::Value;

#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
use truss_core::ctx::Ctx;
#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
use truss_core::error::ProviderError;

/// Send an HTTP request, racing it against context cancellation and applying the
/// context deadline as a per-request timeout. Centralizes error classification so
/// every provider gets the same retry semantics.
#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
pub(crate) async fn send(
    ctx: &Ctx,
    mut rb: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    if let Some(remaining) = ctx.remaining() {
        rb = rb.timeout(remaining);
    }
    let token = ctx.cancel_token();
    tokio::select! {
        _ = token.cancelled() => Err(ProviderError::Cancelled),
        res = rb.send() => res.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Transport(e.to_string())
            }
        }),
    }
}

/// Read a response body as JSON, mapping non-2xx into typed errors (429 →
/// `RateLimited` with `Retry-After`, 5xx/other → `Status`).
#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
pub(crate) async fn read_json(resp: reqwest::Response) -> Result<Value, ProviderError> {
    let resp = ensure_success(resp).await?;
    let body = resp
        .text()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| ProviderError::Decode(e.to_string()))
}

#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
pub(crate) async fn ensure_success(
    resp: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = resp.status();
    if status.as_u16() == 429 {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        return Err(ProviderError::RateLimited {
            retry_after_secs: retry_after,
        });
    }
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp
        .text()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
    Err(ProviderError::Status {
        status: status.as_u16(),
        body,
    })
}

#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
pub(crate) fn push_sse_chunk(buffer: &mut String, chunk: &str) -> Vec<SseFrame> {
    buffer.push_str(chunk);
    let mut frames = Vec::new();

    while let Some(end) = buffer.find("\n\n") {
        let raw = buffer[..end].to_string();
        buffer.drain(..end + 2);
        if let Some(frame) = parse_sse_frame(&raw) {
            frames.push(frame);
        }
    }

    frames
}

#[cfg(any(
    feature = "provider-anthropic",
    feature = "provider-openai",
    feature = "provider-openai-codex"
))]
fn parse_sse_frame(raw: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data = Vec::new();

    for line in raw.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start().to_string());
        }
    }

    if data.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data.join("\n"),
    })
}
