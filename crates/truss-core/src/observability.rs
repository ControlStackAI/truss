//! Tracing/observability setup.
//!
//! Structured logs via `tracing`, filtered by the `RUST_LOG` env var (default
//! `warn` so normal runs stay quiet). The agent's *semantic* events go through
//! [`crate::events`]; this is for diagnostics (retries, transport errors, etc.).

/// Initialize the global tracing subscriber. Safe to call once at startup.
pub fn init() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
