//! The contract — the core primitive of verified delegation.
//!
//! Work enters the harness as a *contract*, not a prompt: an intent, a
//! machine-and-model-checkable definition-of-done, a budget, and enforced
//! constraints. The harness executes it, then independently produces evidence the
//! contract was met (see [`crate::delegation`]). The human reviews the verdict +
//! evidence, not the transcript.
//!
//! Contracts are TOML (see `examples/contract.toml`). Structured `expect_*`
//! fields replace the schema doc's `expect` string for robustness.

use std::path::PathBuf;

use serde::Deserialize;

use crate::approval::ApprovalGatePolicyLayer;
use crate::mcp::McpConfig;
use crate::memory::MemoryConfigLayer;
use crate::sandbox::SandboxConfigLayer;

#[derive(Debug, Deserialize)]
pub struct Contract {
    pub contract: Meta,
    #[serde(default)]
    pub done: Vec<DoneClause>,
    #[serde(default)]
    pub budget: BudgetSpec,
    #[serde(default)]
    pub constraints: ConstraintSpec,
    /// Size of the adversarial reviewer panel for `review` clauses (a strict
    /// majority must say "met" to pass). Default 1; CLI `--reviewers` overrides.
    #[serde(default)]
    pub reviewers: Option<usize>,
    /// Per-contract governed-memory overrides. Layered after global and
    /// per-agent config when building a session.
    #[serde(default)]
    pub memory: MemoryConfigLayer,
    /// Per-contract sandbox overrides. Layered after global and per-agent config
    /// when building a session.
    #[serde(default)]
    pub sandbox: SandboxConfigLayer,
    /// Per-contract approval-gate overrides. Layered after global and
    /// per-agent config when building a durable session.
    #[serde(default)]
    pub approval_gates: ApprovalGatePolicyLayer,
    /// Per-contract MCP servers. Tools are mounted as `mcp:<server>:<tool>`
    /// and still pass through Truss policy, budget, cancellation, and ledger
    /// provenance.
    #[serde(default)]
    pub mcp: McpConfig,
    /// How the work is executed. Default (omitted) = in-loop: Truss drives a
    /// provider. `kind = "delegated-cli"` shells out to a vendor agent CLI on its
    /// own subscription (ADR 0003).
    #[serde(default)]
    pub executor: ExecutorSpec,
}

/// Contract `[executor]` block — selects how the work gets done.
#[derive(Debug, Deserialize, Default)]
pub struct ExecutorSpec {
    /// `"in-loop"` (default) or `"delegated-cli"`.
    pub kind: Option<String>,
    /// For `delegated-cli`: the program to run (e.g. `claude`).
    pub command: Option<String>,
    /// For `delegated-cli`: fixed args (e.g. `["-p", "--output-format", "text"]`).
    #[serde(default)]
    pub args: Vec<String>,
    /// For `delegated-cli`: working directory for the child process.
    pub cwd: Option<PathBuf>,
    /// For `delegated-cli`: env vars stripped from the child so the CLI uses its
    /// own subscription login (e.g. `ANTHROPIC_API_KEY`). We never replay tokens.
    #[serde(default)]
    pub clear_env: Vec<String>,
    /// For `delegated-cli`: `"stdin"` (default) or `"arg"`.
    pub prompt_via: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Meta {
    pub id: String,
    pub title: String,
    /// Plain-language WHY — the anchor the no-stake reviewer judges against.
    pub intent: String,
}

/// One definition-of-done clause. `kind = "check"` is machine-run by the harness;
/// `kind = "review"` is judged by a no-stake reviewer model.
#[derive(Debug, Deserialize, Clone)]
pub struct DoneClause {
    pub kind: String,
    pub desc: String,
    /// For `check`: shell command the harness runs (NOT the executor).
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub expect_exit: Option<i32>,
    #[serde(default)]
    pub expect_contains: Vec<String>,
    /// For `review`: files the reviewer should read for context.
    #[serde(default)]
    pub reviewer_context: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct BudgetSpec {
    pub max_cost_usd: Option<f64>,
    pub max_total_tokens: Option<u64>,
    pub max_steps: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConstraintSpec {
    pub write_scope: Option<Vec<PathBuf>>,
    pub read_scope: Option<Vec<PathBuf>>,
    pub network: Option<String>,
    pub allow_tools: Option<Vec<String>>,
    pub forbidden: Option<Vec<String>>,
    /// System prompt for the executor.
    pub system: Option<String>,
}

impl Contract {
    pub fn from_path(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    /// If the contract delegates execution to a vendor CLI, build it. `None` =
    /// in-loop (Truss drives a provider). Errors if `kind = "delegated-cli"` but
    /// no `command` is given.
    pub fn cli_executor(&self) -> anyhow::Result<Option<crate::executor::CliExecutor>> {
        use crate::executor::{CliExecutor, PromptVia};
        let e = &self.executor;
        match e.kind.as_deref() {
            None | Some("in-loop") => Ok(None),
            Some("delegated-cli") => {
                let command = e.command.clone().ok_or_else(|| {
                    anyhow::anyhow!("[executor] kind = \"delegated-cli\" requires `command`")
                })?;
                let prompt_via = match e.prompt_via.as_deref() {
                    None | Some("stdin") => PromptVia::Stdin,
                    Some("arg") => PromptVia::Arg,
                    Some(other) => {
                        anyhow::bail!(
                            "[executor] prompt_via must be \"stdin\" or \"arg\", got {other:?}"
                        )
                    }
                };
                Ok(Some(CliExecutor {
                    command,
                    args: e.args.clone(),
                    cwd: e.cwd.clone(),
                    clear_env: e.clear_env.clone(),
                    prompt_via,
                }))
            }
            Some(other) => {
                anyhow::bail!(
                    "[executor] kind must be \"in-loop\" or \"delegated-cli\", got {other:?}"
                )
            }
        }
    }

    pub fn budget_limits(&self) -> crate::budget::BudgetLimits {
        crate::budget::BudgetLimits {
            max_total_tokens: self.budget.max_total_tokens,
            max_cost_usd: self.budget.max_cost_usd,
            max_steps: self.budget.max_steps,
        }
    }

    pub fn policy(&self) -> crate::policy::Policy {
        use crate::policy::{NetworkPolicy, Policy};
        let c = &self.constraints;
        let network = match c.network.as_deref() {
            Some("open") => NetworkPolicy::Open,
            Some("allowlist") => NetworkPolicy::Allowlist(Vec::new()),
            _ => NetworkPolicy::None,
        };
        let mut p = Policy {
            write_scope: c.write_scope.clone().unwrap_or_default(),
            read_scope: c.read_scope.clone(),
            network,
            allow_tools: c.allow_tools.clone().map(|v| v.into_iter().collect()),
            ..Default::default()
        };
        if let Some(f) = &c.forbidden {
            p.forbidden_substrings = f.clone();
        }
        p
    }

    /// The prompt handed to the executor: the intent, framed as a task.
    pub fn executor_prompt(&self) -> String {
        format!(
            "You are completing a contracted task. Intent:\n\n{}\n\n\
             Definition of done:\n{}\n\nDo the work. When finished, briefly state what you did.",
            self.contract.intent,
            self.done
                .iter()
                .map(|d| format!("- ({}) {}", d.kind, d.desc))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}
