//! TOML configuration.
//!
//! A config file is optional — the CLI works from flags alone — but it's how a
//! real deployment pins providers, budgets, and policy declaratively. Flags
//! override config where they overlap.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use truss_core::approval::{ApprovalGatePolicy, ApprovalGatePolicyLayer};
use truss_core::budget::BudgetLimits;
use truss_core::error::ProviderError;
use truss_core::mcp::McpConfig;
use truss_core::memory::{MemoryConfig, MemoryConfigLayer};
use truss_core::policy::{NetworkPolicy, Policy};
use truss_core::provider::Provider;
use truss_core::sandbox::{SandboxConfig, SandboxConfigLayer};
use truss_core::tools::WebSearchConfig;
#[cfg(feature = "provider-anthropic")]
use truss_providers::Anthropic;
#[cfg(feature = "provider-openai")]
use truss_providers::OpenAi;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    /// `[agent]` defaults (system / max_tokens / max_steps); CLI flags override.
    #[serde(default)]
    pub agent: AgentConfig,
    /// Global governed-memory defaults.
    #[serde(default)]
    pub memory: MemoryConfigLayer,
    /// Global sandbox defaults.
    #[serde(default)]
    pub sandbox: SandboxConfigLayer,
    /// Global approval-gate defaults.
    #[serde(default)]
    pub approval_gates: ApprovalGatePolicyLayer,
    /// Global MCP server defaults. Contract-scoped MCP servers are appended for
    /// contract runs.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Native tool opt-ins. These are separate from `[policy].allow_tools`: this
    /// block controls registration, while policy controls what the model may use.
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    /// "anthropic" | "openai"
    pub kind: String,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Env var holding the API key (defaults to the provider's conventional var).
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AgentConfig {
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    pub max_steps: Option<usize>,
    pub reasoning: Option<truss_core::provider::ReasoningEffort>,
    pub temperature: Option<f32>,
    /// Per-agent governed-memory overrides.
    #[serde(default)]
    pub memory: MemoryConfigLayer,
    /// Per-agent sandbox overrides.
    #[serde(default)]
    pub sandbox: SandboxConfigLayer,
    /// Per-agent approval-gate overrides.
    #[serde(default)]
    pub approval_gates: ApprovalGatePolicyLayer,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ToolConfig {
    #[serde(default)]
    pub memory: bool,
    #[serde(default)]
    pub git: bool,
    #[serde(default)]
    pub github: bool,
    #[serde(default)]
    pub web: bool,
    #[serde(default)]
    pub web_search: WebSearchConfigLayer,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct WebSearchConfigLayer {
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
}

impl From<WebSearchConfigLayer> for WebSearchConfig {
    fn from(layer: WebSearchConfigLayer) -> Self {
        WebSearchConfig {
            endpoint: layer.endpoint,
            api_key_env: layer.api_key_env,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct BudgetConfig {
    pub max_total_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_steps: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PolicyConfig {
    pub write_scope: Option<Vec<PathBuf>>,
    pub read_scope: Option<Vec<PathBuf>>,
    /// "none" | "open" | "allowlist"
    pub network: Option<String>,
    pub allowlist: Option<Vec<String>>,
    pub allow_tools: Option<Vec<String>>,
    pub forbidden: Option<Vec<String>>,
}

impl Config {
    pub fn from_path(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    /// Build the provider, resolving the API key from the environment. Each
    /// provider arm is compiled only when its Cargo feature is enabled (ADR 0001).
    pub fn build_provider(&self) -> Result<Arc<dyn Provider>, ProviderError> {
        let p = &self.provider;
        #[cfg(not(any(feature = "provider-anthropic", feature = "provider-openai")))]
        let _ = (&p.model, &p.base_url, &p.api_key_env);
        match p.kind.as_str() {
            #[cfg(feature = "provider-anthropic")]
            "anthropic" => {
                let env_var = p.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY");
                let api_key = std::env::var(env_var)
                    .map_err(|_| ProviderError::Config(format!("missing env var {env_var}")))?;
                Ok(Arc::new(Anthropic::new(
                    p.model.clone(),
                    api_key,
                    p.base_url.clone(),
                )))
            }
            #[cfg(feature = "provider-openai")]
            "openai" => {
                let env_var = p.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
                let api_key = std::env::var(env_var)
                    .map_err(|_| ProviderError::Config(format!("missing env var {env_var}")))?;
                Ok(Arc::new(OpenAi::new(
                    p.model.clone(),
                    api_key,
                    p.base_url.clone(),
                )))
            }
            other => Err(ProviderError::Config(format!(
                "provider '{other}' is not available in this build (missing feature)"
            ))),
        }
    }

    pub fn budget_limits(&self) -> BudgetLimits {
        BudgetLimits {
            max_total_tokens: self.budget.max_total_tokens,
            max_cost_usd: self.budget.max_cost_usd,
            max_steps: self.budget.max_steps,
        }
    }

    pub fn memory_config(&self, contract: Option<&MemoryConfigLayer>) -> MemoryConfig {
        MemoryConfig::resolve(Some(&self.memory), Some(&self.agent.memory), contract)
    }

    pub fn sandbox_config(&self, contract: Option<&SandboxConfigLayer>) -> SandboxConfig {
        SandboxConfig::resolve(Some(&self.sandbox), Some(&self.agent.sandbox), contract)
    }

    pub fn approval_gate_policy(
        &self,
        contract: Option<&ApprovalGatePolicyLayer>,
    ) -> ApprovalGatePolicy {
        ApprovalGatePolicy::resolve(
            Some(&self.approval_gates),
            Some(&self.agent.approval_gates),
            contract,
        )
    }

    pub fn policy(&self) -> Policy {
        let pc = &self.policy;
        let network = match pc.network.as_deref() {
            Some("open") => NetworkPolicy::Open,
            Some("allowlist") => NetworkPolicy::Allowlist(pc.allowlist.clone().unwrap_or_default()),
            _ => NetworkPolicy::None,
        };
        let mut policy = Policy {
            write_scope: pc.write_scope.clone().unwrap_or_default(),
            read_scope: pc.read_scope.clone(),
            network,
            allow_tools: pc.allow_tools.clone().map(|v| v.into_iter().collect()),
            ..Default::default()
        };
        if let Some(forbidden) = &pc.forbidden {
            policy.forbidden_substrings = forbidden.clone();
        }
        policy
    }
}
