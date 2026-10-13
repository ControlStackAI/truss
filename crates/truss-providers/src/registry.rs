//! The provider plugin boundary.
//!
//! The CLI (and anything else driving the harness) never names a concrete
//! provider, resolves an API key, or knows a default model. It hands the registry
//! a provider id + a [`ProviderConfig`] and gets back an `Arc<dyn Provider>`.
//!
//! Crucially, **auth resolution lives inside each provider's `build`**, not in the
//! caller. Today every built-in provider resolves an API key from the environment;
//! a future subscription/OAuth provider resolves its own token behind the exact
//! same seam — so adding one never touches `truss-core` or `truss-cli`. Adding a
//! provider is: a new module + a cargo feature + two lines here (one in
//! [`available`], one match arm in [`build`]).

use std::sync::Arc;

use truss_core::provider::Provider;

/// What the caller hands the registry to construct a provider. Intentionally
/// minimal — it carries *intent* (which model, which endpoint), not credentials.
/// Credential/subscription resolution is the provider's job (see module docs).
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// Model id. `None` means "use the provider's default" (providers that have
    /// no sensible default return [`BuildError::Config`]).
    pub model: Option<String>,
    /// Override the API base URL (e.g. a local OpenAI-compatible server).
    pub base_url: Option<String>,
}

/// Why a provider couldn't be constructed. Kept separate from transport/runtime
/// errors: this is "we couldn't even start", surfaced before any model call.
#[derive(Debug, Clone)]
pub enum BuildError {
    /// No provider with this id is compiled into this build.
    Unknown {
        name: String,
        available: Vec<&'static str>,
    },
    /// Credentials are missing or unusable (e.g. an env var isn't set).
    Auth(String),
    /// The request is structurally invalid for this provider (e.g. a model is
    /// required but none was given).
    Config(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Unknown { name, available } => {
                if available.is_empty() {
                    write!(
                        f,
                        "provider '{name}' is not available: this build has no providers compiled in"
                    )
                } else {
                    write!(
                        f,
                        "provider '{name}' is not available in this build (have: {})",
                        available.join(", ")
                    )
                }
            }
            BuildError::Auth(msg) => write!(f, "{msg}"),
            BuildError::Config(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Provider ids compiled into this build, in stable order. Drives `--help`,
/// error messages, and the "available providers" listing.
#[allow(clippy::vec_init_then_push)] // arms are cfg-gated; a vec! literal can't express that
pub fn available() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut v: Vec<&'static str> = Vec::new();
    #[cfg(feature = "provider-anthropic")]
    v.push("anthropic");
    #[cfg(feature = "provider-openai")]
    v.push("openai");
    #[cfg(feature = "provider-openai-codex")]
    v.push("openai-codex");
    v
}

/// Construct a provider by id. The provider resolves its own auth + defaults.
#[cfg_attr(
    not(any(
        feature = "provider-anthropic",
        feature = "provider-openai",
        feature = "provider-openai-codex"
    )),
    allow(unused_variables)
)]
pub fn build(name: &str, cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, BuildError> {
    match name {
        #[cfg(feature = "provider-anthropic")]
        "anthropic" => crate::anthropic::build(cfg),
        #[cfg(feature = "provider-openai")]
        "openai" => crate::openai::build(cfg),
        #[cfg(feature = "provider-openai-codex")]
        "openai-codex" => crate::openai_codex::build(cfg),
        other => Err(BuildError::Unknown {
            name: other.to_string(),
            available: available(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `Arc<dyn Provider>` isn't `Debug`, so we can't `.unwrap_err()` — match instead.
    fn expect_err(r: Result<Arc<dyn Provider>, BuildError>) -> BuildError {
        match r {
            Ok(_) => panic!("expected an error, got a provider"),
            Err(e) => e,
        }
    }

    #[test]
    fn unknown_provider_lists_whats_available() {
        let err = expect_err(build("does-not-exist", &ProviderConfig::default()));
        match &err {
            BuildError::Unknown { name, available } => {
                assert_eq!(name, "does-not-exist");
                // Under default features both providers are compiled in.
                assert!(available.contains(&"anthropic"));
                assert!(available.contains(&"openai"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        // The message names the alternatives so the user can self-correct.
        assert!(err.to_string().contains("anthropic"));
    }

    #[test]
    #[cfg(feature = "provider-openai")]
    fn openai_requires_a_model() {
        // No model + no network: must fail fast with a Config error, not panic
        // or hit the wire.
        let err = expect_err(build("openai", &ProviderConfig::default()));
        assert!(matches!(err, BuildError::Config(_)), "got {err:?}");
    }

    #[test]
    fn available_is_stable_and_nonempty_by_default() {
        let a = available();
        assert_eq!(a, available(), "ordering must be deterministic");
        assert!(!a.is_empty(), "default build should compile in providers");
    }
}
