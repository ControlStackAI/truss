//! Typed error taxonomy.
//!
//! The whole harness distinguishes **retryable** failures (transient HTTP, rate
//! limits, timeouts) from **fatal** ones (bad request, decode failure) from
//! **policy** failures (the agent tried something it isn't allowed to do). That
//! distinction is what lets [`crate::retry`] act automatically and lets a human
//! reviewer trust that a "done" verdict wasn't a silently-swallowed error.
//!
//! `anyhow` is intentionally gone from the hot path — string errors can't be
//! classified, and classification is the point.

use serde_json::Value;
use thiserror::Error;

/// Failures from a [`crate::provider::Provider`] call.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Transport-level failure (connection reset, DNS, TLS). Retryable.
    #[error("transport error: {0}")]
    Transport(String),

    /// HTTP 429. Retryable; honor `retry_after_secs` if present.
    #[error("rate limited (retry after {retry_after_secs:?}s)")]
    RateLimited { retry_after_secs: Option<u64> },

    /// Non-success HTTP status with the response body.
    #[error("provider returned {status}: {body}")]
    Status { status: u16, body: String },

    /// The request did not complete before its deadline. Retryable.
    #[error("request timed out")]
    Timeout,

    /// The context was cancelled mid-flight. Not retryable — it was deliberate.
    #[error("cancelled")]
    Cancelled,

    /// Response body could not be parsed into our types. Fatal.
    #[error("decode error: {0}")]
    Decode(String),

    /// Missing/invalid configuration (e.g. no API key). Fatal.
    #[error("configuration error: {0}")]
    Config(String),
}

impl ProviderError {
    /// Whether [`crate::retry`] should transparently retry this error.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Transport(_)
            | ProviderError::RateLimited { .. }
            | ProviderError::Timeout => true,
            ProviderError::Status { status, .. } => *status >= 500 || *status == 429,
            ProviderError::Cancelled | ProviderError::Decode(_) | ProviderError::Config(_) => false,
        }
    }

    /// Server-suggested backoff, if any.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimited { retry_after_secs } => *retry_after_secs,
            _ => None,
        }
    }
}

/// Failures from a [`crate::tools::Tool`] call.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The tool was blocked by [`crate::policy::Policy`] (write scope, network,
    /// forbidden op, disallowed tool). Surfaced to the model so it can adapt.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("cancelled")]
    Cancelled,

    /// The tool started durable external work and needs the run to suspend until
    /// a signal arrives. Durable execution intercepts this; non-durable callers
    /// see it as an explicit pending state instead of a fake failure.
    #[error("tool suspended awaiting signal '{signal_name}': {reason}")]
    Suspend {
        signal_name: String,
        reason: String,
        resume_payload: Value,
    },

    #[error("execution failed: {0}")]
    Execution(String),
}

/// Top-level error for an agent run.
#[derive(Debug, Error)]
pub enum HarnessError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error(transparent)]
    Tool(#[from] ToolError),

    /// A budget ceiling (tokens, cost, or steps) was hit.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),

    /// The run's context was cancelled (Ctrl-C, deadline, or explicit cancel).
    #[error("run cancelled")]
    Cancelled,

    #[error("reached max steps ({0}) without a final answer")]
    MaxSteps(usize),

    #[error("{0}")]
    Other(String),
}
