//! `truss` CLI — drive the harness from the terminal.
//!
//! Examples:
//!   truss "explain quaternions in one sentence"
//!   truss --tools --workdir . "read Cargo.toml and summarize the deps"
//!   truss --provider openai --base-url http://localhost:11434/v1 \
//!                   --model llama3 "hello"
//!   truss --budget-usd 0.50 --timeout 60 --log run.jsonl "..."
//!
//! Ctrl-C cancels a run mid-flight: the in-flight model call and any running tool
//! (including child processes) are aborted, and the partial event log is flushed.

use std::io::Write;
#[cfg(feature = "swebench-live")]
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::{collections::HashMap, sync::Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;

mod config;
#[cfg(feature = "tui")]
mod tui;

use crate::config::Config;
use truss_core::agent::Agent;
use truss_core::approval::{ApprovalGatePolicy, ApprovalGatePolicyLayer};
use truss_core::provider::ReasoningEffort;
#[cfg(feature = "swebench-live")]
use truss_core::provider::{CompletionRequest, CompletionResponse, StopReason, Usage};

/// CLI mirror of [`ReasoningEffort`] (keeps `clap` out of the core crate).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ReasoningArg {
    Minimal,
    Low,
    Medium,
    High,
}

impl From<ReasoningArg> for ReasoningEffort {
    fn from(a: ReasoningArg) -> Self {
        match a {
            ReasoningArg::Minimal => ReasoningEffort::Minimal,
            ReasoningArg::Low => ReasoningEffort::Low,
            ReasoningArg::Medium => ReasoningEffort::Medium,
            ReasoningArg::High => ReasoningEffort::High,
        }
    }
}

/// CLI mirror of [`CompactionMode`] (keeps `clap` out of the core crate).
#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompactionModeArg {
    Off,
    Summarize,
    Evict,
}

impl From<CompactionModeArg> for CompactionMode {
    fn from(a: CompactionModeArg) -> Self {
        match a {
            CompactionModeArg::Off => CompactionMode::Off,
            CompactionModeArg::Summarize => CompactionMode::Summarize,
            CompactionModeArg::Evict => CompactionMode::Evict,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EffectiveBudgetOverrideArg {
    model: String,
    budget: usize,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum ServeMcpTransport {
    Stdio,
    Http,
}

use truss_core::anchor::{Anchor, FileTransparencyLog};
use truss_core::budget::BudgetMeter;
use truss_core::contract::Contract;
use truss_core::ctx::Ctx;
use truss_core::delegation;
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::HarnessError;
use truss_core::events::AgentEvent;
use truss_core::executor::Executor;
use truss_core::identity::Signer;
use truss_core::ledger::{
    LedgerEntryPayload, LedgerReader, LedgerRecall, LedgerSessionSummary, LedgerShow,
    LedgerVerdictBundle, LedgerVerifyReport, VerificationStatus, LEDGER_SCHEMA_VERSION,
};
use truss_core::mcp::{McpJournal, McpServerConfig, McpTransport};
use truss_core::mcp_server::{
    ContractStatusRequest, ContractStatusResponse, LedgerBundleRef, LedgerReadRequest,
    McpServerError, RunContractRequest, RunContractResponse, TrussMcpBackend, TrussMcpServer,
};
use truss_core::memory::{
    AiMemoryStore, CompactionMode, DeterministicEmbedder, EntryKind, LedgerRef, MemoryConfig,
    MemoryConfigLayer, MemoryCursor, MemoryDelta, MemoryDraft, MemoryEmbeddingBackend, MemoryEntry,
    MemoryError, MemoryId, MemoryQuery, MemoryRecallMode, MemoryStore, MemoryStoreBackend,
    MemoryStoreOptions, PolicyDraft, PolicyId, PolicyRecord, SqliteMemoryStore, Trust,
};
use truss_core::policy::Policy;
use truss_core::provider::{Provider, ToolDef};
use truss_core::retry::RetryPolicy;
use truss_core::sandbox::{sandbox_from_config, SandboxConfig, SandboxConfigLayer, SandboxMode};
use truss_core::state::RunLog;
use truss_core::store::{HumanReviewDecision, HumanReviewEntry, Store};
use truss_core::tools::ToolRegistry;
use truss_providers::ProviderConfig;

#[derive(Parser, Debug)]
#[command(
    name = "truss",
    about = "Truss — a durable, verifiable agent runtime: contracts, signed ledger, verified delegation.",
    version
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Optional TOML config (provider/budget/policy). Flags override it.
    #[arg(long)]
    config: Option<String>,

    /// Provider: `anthropic` or `openai` (the latter also covers local
    /// OpenAI-compatible servers like Ollama via --base-url).
    #[arg(long, default_value = "anthropic")]
    provider: String,

    /// Model id (defaults to claude-opus-4-8 for anthropic; required for openai).
    #[arg(long)]
    model: Option<String>,

    /// Override the API base URL (e.g. http://localhost:11434/v1 for Ollama).
    #[arg(long)]
    base_url: Option<String>,

    /// Optional system prompt.
    #[arg(long)]
    system: Option<String>,

    /// Max agent steps (model calls) before giving up. [config: agent.max_steps; default 12]
    #[arg(long)]
    max_steps: Option<usize>,

    /// Max output tokens per model call. [config: agent.max_tokens; default 4096]
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Reasoning / thinking level for models that support it (OpenAI effort,
    /// Anthropic extended-thinking budget). [config: agent.reasoning]
    #[arg(long, value_enum)]
    reasoning: Option<ReasoningArg>,

    /// Sampling temperature. [config: agent.temperature] (ignored by Anthropic
    /// when reasoning/thinking is enabled).
    #[arg(long)]
    temperature: Option<f32>,

    /// Governed context compaction mode. [config: memory.compaction.mode; default off]
    #[arg(long, value_enum)]
    compaction_mode: Option<CompactionModeArg>,

    /// Context pressure threshold for governed compaction, 0.0 through 1.0.
    /// [config: memory.compaction.threshold; default 0.70]
    #[arg(long, value_parser = parse_unit_interval)]
    compaction_threshold: Option<f64>,

    /// Minimum verbatim tail messages retained after governed compaction.
    /// [config: memory.compaction.min_tail; default 6]
    #[arg(long)]
    compaction_min_tail: Option<usize>,

    /// Enable CC-3 compact-ahead anticipation. [config: memory.compaction.anticipate]
    #[arg(long)]
    compaction_anticipate: bool,

    /// EC-1 effective-context budget ratio, 0.0 through 1.0.
    /// [config: memory.effective_budget.ratio; default 0.60]
    #[arg(long, value_parser = parse_effective_budget_ratio)]
    effective_budget_ratio: Option<f64>,

    /// EC-1 per-model effective budget override as MODEL=TOKENS.
    /// [config: memory.effective_budget.overrides]
    #[arg(long, value_parser = parse_effective_budget_override)]
    effective_budget_override: Vec<EffectiveBudgetOverrideArg>,

    /// Register the built-in toolset (read_file, write_file, list_dir, bash).
    #[arg(long)]
    tools: bool,

    /// Register native git tools (git_status, git_diff, git_branch, git_commit).
    #[arg(long)]
    git_tools: bool,

    /// Register native GitHub tools backed by gh (PRs, issues, checks).
    #[arg(long)]
    github_tools: bool,

    /// Register native web tools (web_fetch, web_search).
    #[arg(long)]
    web_tools: bool,

    /// Register read-only native memory tools (memory_query, memory_view).
    #[arg(long)]
    memory_tools: bool,

    /// Enable the native governed memory tool for this durable session.
    #[arg(long)]
    memory_tool: bool,

    /// Enable the store-backed model-facing spawn tool for durable/contract runs.
    #[arg(long)]
    spawn_tool: bool,

    /// Confine reads+writes to this directory (sets a workdir policy).
    #[arg(long)]
    workdir: Option<String>,

    /// Hard cost ceiling in USD for the whole run.
    #[arg(long)]
    budget_usd: Option<f64>,

    /// Hard token ceiling (input+output) for the whole run.
    #[arg(long)]
    budget_tokens: Option<u64>,

    /// Wall-clock deadline in seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Append the JSONL event/audit log to this file.
    #[arg(long)]
    log: Option<String>,

    /// Print every event as it happens (default prints only the final answer).
    #[arg(long)]
    verbose: bool,

    /// Enable durable execution (SQLite journal + snapshots + resume).
    #[arg(long)]
    durable: bool,

    /// Durable store path.
    #[arg(long, default_value = "harness.db")]
    db: String,

    /// Session id for a durable run (generated if omitted).
    #[arg(long)]
    session: Option<String>,

    /// Resume a suspended/crashed durable session by id (implies --durable).
    #[arg(long)]
    resume: Option<String>,

    /// Verify the hash chain + signatures of a durable session and exit.
    #[arg(long)]
    verify: Option<String>,

    /// Path to the ed25519 signing identity (created if missing).
    #[arg(long, default_value = "harness-identity.key")]
    key: String,

    /// Pin the expected signing public key (hex) when verifying.
    #[arg(long)]
    expect_key: Option<String>,

    /// Deliver a named signal to --session (durably recorded) and exit.
    #[arg(long)]
    signal: Option<String>,

    /// Optional payload for --signal.
    #[arg(long)]
    payload: Option<String>,

    /// Run the resume daemon: poll for suspended sessions whose await is
    /// satisfied (timer elapsed / signal delivered) and resume them.
    #[arg(long)]
    daemon: bool,

    /// Daemon poll interval in seconds.
    #[arg(long, default_value_t = 5)]
    poll_secs: u64,

    /// Anchor a session's head hash to the transparency log and exit.
    #[arg(long)]
    anchor: Option<String>,

    /// Path to the append-only transparency log.
    #[arg(long, default_value = "anchors.jsonl")]
    anchor_log: String,

    /// Verify the transparency log's internal hash chain and exit.
    #[arg(long)]
    verify_anchor_log: bool,

    /// If > 0, compact history to Head + summary + this many recent turns per call.
    #[arg(long, default_value_t = 0)]
    max_context_msgs: usize,

    /// Run a contract (verified delegation): execute it, then independently
    /// produce evidence + a signed verdict. Path to a TOML contract.
    #[arg(long)]
    contract: Option<String>,

    /// Model the no-stake reviewer uses (defaults to --model; ideally different).
    #[arg(long)]
    reviewer_model: Option<String>,

    /// Size of the adversarial reviewer panel (a strict majority must agree a
    /// `review` clause is met). Overrides the contract's `reviewers`. [default 1]
    #[arg(long)]
    reviewers: Option<usize>,

    /// The prompt (everything after the flags).
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read signed durable ledger proofs.
    Ledger(LedgerArgs),
    /// Record signed human review decisions, or launch the interactive TUI
    /// (`truss review` with no decision opens the interactive surface).
    Review(ReviewArgs),
    /// Launch the interactive review TUI (requires `--features tui`).
    Tui(TuiArgs),
    /// Inspect and probe configured MCP servers.
    Mcp(McpArgs),
    /// Serve Truss as an MCP server for contract-level delegation.
    ServeMcp(ServeMcpArgs),
    /// Import and inspect governed playbook drafts.
    Playbook(PlaybookArgs),
    /// Maintain governed memory indexes.
    Memory(MemoryArgs),
    /// Run a paired SI-4 learning-on/off eval for a contract.
    Eval(EvalArgs),
    /// Run SWE-bench Verified from local cached data and prediction patches.
    #[cfg(feature = "swebench-live")]
    Swebench(SweBenchArgs),
}

#[derive(Parser, Debug)]
struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Parser, Debug)]
struct ServeMcpArgs {
    /// Transport to serve. Only stdio is implemented in this slice.
    #[arg(long, value_enum, default_value_t = ServeMcpTransport::Stdio)]
    transport: ServeMcpTransport,
    /// Tenant for the single stdio caller principal.
    #[arg(long, default_value = "local")]
    principal_tenant: String,
    /// User for the single stdio caller principal.
    #[arg(long, default_value = "local")]
    principal_user: String,
}

#[derive(Parser, Debug)]
struct PlaybookArgs {
    #[command(subcommand)]
    command: PlaybookCommand,
}

#[derive(Parser, Debug)]
struct MemoryArgs {
    #[command(subcommand)]
    command: MemoryCommand,
}

#[derive(Subcommand, Debug)]
enum MemoryCommand {
    /// Backfill missing embeddings for existing memory entries.
    Reindex {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PlaybookCommand {
    /// Import a markdown/SKILL.md file as an unverified governed playbook draft.
    Import {
        /// Markdown or SKILL.md path.
        path: String,
        /// Stable supersede key. Defaults to the file stem.
        #[arg(long)]
        key: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List playbooks stored in governed memory.
    List {
        /// Show only verified playbooks.
        #[arg(long)]
        verified_only: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one playbook by memory id or key.
    Show {
        /// Memory id or stable key.
        id_or_key: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Promote an unverified playbook draft through the N-reviewer panel or paired eval.
    Promote {
        /// Memory id or stable key.
        id_or_key: String,
        /// Use paired eval evidence instead of the local reviewer panel.
        #[arg(long = "eval")]
        eval: bool,
        /// TOML contract to evaluate when --eval is enabled.
        #[arg(long)]
        contract: Option<String>,
        /// Number of paired eval repeats per arm.
        #[arg(long)]
        repeats: Option<usize>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Demote a playbook to stale and record a signed revocation event.
    Revoke {
        /// Memory id or stable key.
        id_or_key: String,
        /// Human-readable revocation reason.
        #[arg(long)]
        reason: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum McpCommand {
    /// List configured MCP servers without launching them.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Connect to one configured MCP server and list advertised tools.
    Probe {
        /// MCP server name from [[mcp.servers]].
        server: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Parser, Debug)]
struct LedgerArgs {
    #[command(subcommand)]
    command: LedgerCommand,
}

#[derive(Parser, Debug)]
struct ReviewArgs {
    /// Decision subcommand. Omit it to launch the interactive review TUI
    /// (requires the binary be built with `--features tui`).
    #[command(subcommand)]
    command: Option<ReviewCommand>,
    /// Durable store path for the TUI (when no decision subcommand is given).
    #[arg(long)]
    db: Option<String>,
}

#[derive(Parser, Debug)]
struct TuiArgs {
    /// Durable store path.
    #[arg(long)]
    db: Option<String>,
}

#[derive(Subcommand, Debug)]
enum ReviewCommand {
    /// Approve the current reviewable head.
    Approve(ReviewDecisionArgs),
    /// Reject the current reviewable head.
    Reject(ReviewDecisionArgs),
    /// Request changes for the current reviewable head.
    NeedsChanges(ReviewDecisionArgs),
}

#[derive(Parser, Debug)]
struct ReviewDecisionArgs {
    /// Durable session/run id.
    session: String,
    /// Optional human review note.
    #[arg(long)]
    note: Option<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
    /// Durable store path.
    #[arg(long)]
    db: Option<String>,
    /// Path to the ed25519 signing identity.
    #[arg(long)]
    key: Option<String>,
}

#[derive(Subcommand, Debug)]
enum LedgerCommand {
    /// List durable ledger sessions.
    List {
        /// Emit the stable versioned JSON schema.
        #[arg(long)]
        json: bool,
    },
    /// Show entries for one ledger session.
    Show {
        /// Durable session/run id.
        run_id: String,
        /// Emit the stable versioned JSON schema.
        #[arg(long)]
        json: bool,
    },
    /// Get the full payload for one ledger entry.
    Get {
        /// Durable session/run id.
        run_id: String,
        /// Entry sequence number.
        seq: i64,
        /// Emit the stable versioned JSON schema.
        #[arg(long)]
        json: bool,
    },
    /// Render the verdict bundle for one completed run.
    Verdict {
        /// Durable session/run id.
        run_id: String,
        /// Emit the stable versioned JSON schema.
        #[arg(long)]
        json: bool,
    },
    /// Export a portable signed evidence bundle for offline verification.
    Export {
        /// Durable session/run id.
        run_id: String,
    },
    /// Recall run memory entries and compacted context summaries.
    Recall {
        /// Durable session/run id.
        run_id: String,
        /// Expand one ContextSummary memory id back to its elided journal range.
        #[arg(long)]
        summary: Option<String>,
        /// Emit the stable versioned JSON schema.
        #[arg(long)]
        json: bool,
    },
    /// Verify one durable run's hash chain and pinned signatures.
    Verify {
        /// Durable session/run id.
        run_id: String,
        /// Emit the stable versioned JSON schema.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Parser, Debug)]
struct EvalArgs {
    /// Path to the TOML contract evaluated by both paired arms.
    #[arg(long)]
    contract: String,

    /// Number of repeats per arm.
    #[arg(long, default_value_t = 1)]
    repeats: usize,
}

#[cfg(feature = "swebench-live")]
#[derive(Parser, Debug)]
struct SweBenchArgs {
    /// Local JSON/JSONL export of `princeton-nlp/SWE-bench_Verified`.
    #[arg(long)]
    dataset: String,

    /// Root containing per-instance checkouts named after sanitized instance ids.
    #[arg(long)]
    checkout_root: String,

    /// Patch source mode: `predictions` grades an existing JSONL; `in-loop`
    /// first lets Truss solve each task with its own provider/tool loop.
    #[arg(long, value_enum, default_value_t = SweBenchSolveMode::Predictions)]
    solve: SweBenchSolveMode,

    /// Official SWE-bench predictions JSONL with `instance_id` + `model_patch`.
    /// Required for `--solve predictions`; optional output path for `--solve in-loop`.
    #[arg(long)]
    predictions: Option<String>,

    /// Limit the number of dataset records for smoke runs.
    #[arg(long)]
    limit: Option<usize>,

    /// Number of paired eval repeats per task.
    #[arg(long, default_value_t = 1)]
    repeats: usize,

    /// Python executable with the SWE-bench harness installed.
    #[arg(long, default_value = "python")]
    python: String,

    /// SWE-bench harness run id.
    #[arg(long, default_value = "truss-swebench")]
    run_id: String,

    /// Max workers passed to the SWE-bench harness.
    #[arg(long, default_value_t = 1)]
    max_workers: usize,
}

#[cfg(feature = "swebench-live")]
#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
enum SweBenchSolveMode {
    Predictions,
    InLoop,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct McpServerSummary {
    name: String,
    transport: String,
    command: Option<String>,
    url: Option<String>,
    allow_tools: Option<Vec<String>>,
    headers: Vec<String>,
    env_headers: Vec<McpEnvHeaderSummary>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct McpEnvHeaderSummary {
    header: String,
    env_var: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct McpListOutput {
    servers: Vec<McpServerSummary>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct McpToolSummary {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct McpProbeOutput {
    server: McpServerSummary,
    tools: Vec<McpToolSummary>,
}

async fn handle_mcp_command(
    command: &McpCommand,
    config: Option<&Config>,
    ctx: &Ctx,
) -> Result<()> {
    match command {
        McpCommand::List { json } => {
            let output = mcp_list_output(config);
            if *json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if output.servers.is_empty() {
                println!("No MCP servers configured.");
            } else {
                for server in output.servers {
                    println!(
                        "{}\t{}\t{}",
                        server.name,
                        server.transport,
                        server
                            .command
                            .or(server.url)
                            .unwrap_or_else(|| "<missing endpoint>".into())
                    );
                }
            }
        }
        McpCommand::Probe { server, json } => {
            let output = mcp_probe_output(config, ctx, server).await?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "{}\t{}\t{} tool(s)",
                    output.server.name,
                    output.server.transport,
                    output.tools.len()
                );
                for tool in output.tools {
                    println!("{}\t{}", tool.name, tool.description);
                }
            }
        }
    }
    Ok(())
}

fn mcp_list_output(config: Option<&Config>) -> McpListOutput {
    McpListOutput {
        servers: mcp_servers_for(config, None)
            .iter()
            .map(mcp_server_summary)
            .collect(),
    }
}

async fn mcp_probe_output(
    config: Option<&Config>,
    ctx: &Ctx,
    name: &str,
) -> Result<McpProbeOutput> {
    let server = mcp_servers_for(config, None)
        .into_iter()
        .find(|server| server.name == name)
        .ok_or_else(|| anyhow!("MCP server '{name}' not found"))?;
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_servers(ctx, std::slice::from_ref(&server), None)
        .await
        .map_err(|e| anyhow!(e.to_string()))?;
    let mut tools: Vec<McpToolSummary> = registry.defs().iter().map(mcp_tool_summary).collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(McpProbeOutput {
        server: mcp_server_summary(&server),
        tools,
    })
}

fn mcp_server_summary(server: &McpServerConfig) -> McpServerSummary {
    McpServerSummary {
        name: server.name.clone(),
        transport: mcp_transport_name(server.transport).into(),
        command: server.command.clone(),
        url: server.url.clone(),
        allow_tools: server.allow_tools.clone(),
        headers: server.headers.keys().cloned().collect(),
        env_headers: server
            .env_headers
            .iter()
            .map(|(header, env_var)| McpEnvHeaderSummary {
                header: header.clone(),
                env_var: env_var.clone(),
            })
            .collect(),
    }
}

fn mcp_tool_summary(tool: &ToolDef) -> McpToolSummary {
    McpToolSummary {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
    }
}

fn mcp_transport_name(transport: McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
    }
}

/// Build the primary provider via the registry. The CLI never names a concrete
/// provider or resolves a key — `truss_providers::build` dispatches by id and the
/// provider resolves its own auth + default model.
fn build_provider(args: &Args) -> Result<Arc<dyn Provider>> {
    let cfg = ProviderConfig {
        model: args.model.clone(),
        base_url: args.base_url.clone(),
    };
    truss_providers::build(&args.provider, &cfg).map_err(|e| anyhow!(e.to_string()))
}

/// Build a provider with an explicit model (used for the no-stake reviewer panel).
fn build_named_provider(args: &Args, model: &str) -> Result<Arc<dyn Provider>> {
    let cfg = ProviderConfig {
        model: Some(model.to_string()),
        base_url: args.base_url.clone(),
    };
    truss_providers::build(&args.provider, &cfg).map_err(|e| anyhow!(e.to_string()))
}

fn parse_unit_interval(raw: &str) -> std::result::Result<f64, String> {
    let value = raw
        .parse::<f64>()
        .map_err(|e| format!("expected a number between 0.0 and 1.0: {e}"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "compaction threshold must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn parse_effective_budget_ratio(raw: &str) -> std::result::Result<f64, String> {
    let value = raw
        .parse::<f64>()
        .map_err(|e| format!("expected a number between 0.0 and 1.0: {e}"))?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "effective budget ratio must be between 0.0 and 1.0, got {value}"
        ))
    }
}

fn parse_effective_budget_override(
    raw: &str,
) -> std::result::Result<EffectiveBudgetOverrideArg, String> {
    let (model, budget) = raw
        .split_once('=')
        .ok_or_else(|| "expected MODEL=TOKENS".to_string())?;
    if model.trim().is_empty() {
        return Err("model id must not be empty".into());
    }
    let budget = budget
        .parse::<usize>()
        .map_err(|e| format!("expected positive token budget: {e}"))?;
    if budget == 0 {
        return Err("token budget must be greater than zero".into());
    }
    Ok(EffectiveBudgetOverrideArg {
        model: model.to_string(),
        budget,
    })
}

fn resolved_memory_config(
    config: Option<&Config>,
    contract: Option<&MemoryConfigLayer>,
    args: &Args,
) -> MemoryConfig {
    let mut memory = config
        .map(|c| c.memory_config(contract))
        .unwrap_or_else(|| MemoryConfig::resolve(None, None, contract));
    apply_compaction_flag_overrides(args, &mut memory);
    memory
}

fn memory_store_from_config(store: &Store, config: &MemoryConfig) -> Result<Arc<dyn MemoryStore>> {
    match config.store_backend {
        MemoryStoreBackend::Sqlite => {
            let options = memory_store_options(config)?;
            Ok(Arc::new(
                SqliteMemoryStore::from_store_with_options(store, options)
                    .map_err(|e| anyhow!(e.to_string()))?,
            ))
        }
        MemoryStoreBackend::AiMemory => {
            if config.recall != MemoryRecallMode::Exact {
                return Err(anyhow!(
                    "ai-memory currently supports exact recall only; set memory.recall = \"exact\""
                ));
            }
            let socket = config.ai_memory_socket.as_ref().ok_or_else(|| {
                anyhow!("memory.store_backend = \"ai_memory\" requires memory.ai_memory_socket")
            })?;
            Ok(Arc::new(
                AiMemoryStore::new(socket).map_err(|e| anyhow!(e.to_string()))?,
            ))
        }
    }
}

fn memory_store_options(config: &MemoryConfig) -> Result<MemoryStoreOptions> {
    match config.recall {
        MemoryRecallMode::Exact => Ok(MemoryStoreOptions::default()),
        MemoryRecallMode::Semantic | MemoryRecallMode::Hybrid => {
            let embedder: Arc<dyn truss_core::memory::Embedder> = match config.embedding_backend {
                MemoryEmbeddingBackend::Deterministic => {
                    Arc::new(DeterministicEmbedder::new(config.embedding_dims))
                }
                MemoryEmbeddingBackend::OpenAi => build_openai_embedder(config)?,
            };
            Ok(match config.recall {
                MemoryRecallMode::Semantic => MemoryStoreOptions::semantic(embedder),
                MemoryRecallMode::Hybrid => MemoryStoreOptions::hybrid(embedder),
                MemoryRecallMode::Exact => unreachable!(),
            })
        }
    }
}

#[cfg(feature = "provider-openai")]
fn build_openai_embedder(config: &MemoryConfig) -> Result<Arc<dyn truss_core::memory::Embedder>> {
    Ok(Arc::new(truss_providers::OpenAiEmbedder::from_env(
        config.embedding_model.clone(),
        config.embedding_base_url.clone(),
        &config.embedding_api_key_env,
        config.embedding_dims,
    )?))
}

#[cfg(not(feature = "provider-openai"))]
fn build_openai_embedder(_config: &MemoryConfig) -> Result<Arc<dyn truss_core::memory::Embedder>> {
    Err(anyhow!(
        "OpenAI memory embeddings require the truss-cli provider-openai feature"
    ))
}

fn handle_memory_command(
    args: &Args,
    command: &MemoryCommand,
    config: Option<&Config>,
    mut out: impl Write,
) -> Result<()> {
    match command {
        MemoryCommand::Reindex { json } => {
            let mut memory_config = resolved_memory_config(config, None, args);
            memory_config.recall = MemoryRecallMode::Semantic;
            if memory_config.store_backend != MemoryStoreBackend::Sqlite {
                return Err(anyhow!(
                    "memory reindex is only available with memory.store_backend = \"sqlite\""
                ));
            }
            let signer =
                Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
            let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
            let memory = SqliteMemoryStore::from_store_with_options(
                &store,
                memory_store_options(&memory_config)?,
            )
            .map_err(|e| anyhow!(e.to_string()))?;
            let summary = memory.reindex_missing_embeddings()?;
            if *json {
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "scanned": summary.scanned,
                        "embedded": summary.embedded,
                        "embedding_backend": memory_config.embedding_backend,
                        "embedding_model": memory_config.embedding_model,
                        "embedding_dims": memory_config.embedding_dims,
                    }))?
                )?;
            } else {
                writeln!(
                    out,
                    "reindexed memory embeddings: scanned {}, embedded {}",
                    summary.scanned, summary.embedded
                )?;
            }
        }
    }
    Ok(())
}

fn resolved_sandbox_config(
    config: Option<&Config>,
    contract: Option<&SandboxConfigLayer>,
) -> SandboxConfig {
    config
        .map(|c| c.sandbox_config(contract))
        .unwrap_or_else(|| SandboxConfig::resolve(None, None, contract))
}

fn resolved_approval_gate_policy(
    config: Option<&Config>,
    contract: Option<&ApprovalGatePolicyLayer>,
) -> ApprovalGatePolicy {
    config
        .map(|c| c.approval_gate_policy(contract))
        .unwrap_or_else(|| ApprovalGatePolicy::resolve(None, None, contract))
}

fn resolved_policy(args: &Args, config: Option<&Config>) -> Policy {
    if let Some(wd) = &args.workdir {
        Policy::workdir(wd)
    } else if let Some(c) = config {
        c.policy()
    } else {
        Policy::default()
    }
}

fn default_checkout(args: &Args, policy: &Policy) -> std::path::PathBuf {
    args.workdir
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| policy.write_scope.first().cloned())
        .or_else(|| {
            policy
                .read_scope
                .as_ref()
                .and_then(|scopes| scopes.first().cloned())
        })
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

fn declared_test_command(contract: &Contract) -> Option<String> {
    contract
        .done
        .iter()
        .find(|clause| clause.kind == "check")
        .and_then(|clause| clause.run.clone())
}

fn allow_self_validation_tools(policy: &mut Policy) {
    if let Some(allow_tools) = &mut policy.allow_tools {
        allow_tools.insert("self_check".into());
        allow_tools.insert("run_tests".into());
    }
}

fn apply_compaction_flag_overrides(args: &Args, memory: &mut MemoryConfig) {
    if let Some(mode) = args.compaction_mode {
        memory.compaction.mode = mode.into();
    }
    if let Some(threshold) = args.compaction_threshold {
        memory.compaction.threshold = threshold;
    }
    if let Some(min_tail) = args.compaction_min_tail {
        memory.compaction.min_tail = min_tail;
    }
    if args.compaction_anticipate {
        memory.compaction.anticipate = true;
    }
    if let Some(ratio) = args.effective_budget_ratio {
        memory.effective_budget.ratio = ratio;
    }
    for override_arg in &args.effective_budget_override {
        memory
            .effective_budget
            .overrides
            .insert(override_arg.model.clone(), override_arg.budget);
    }
}

#[derive(Serialize)]
struct LedgerListJson {
    schema_version: &'static str,
    command: &'static str,
    sessions: Vec<LedgerSessionSummary>,
}

#[derive(Serialize)]
struct LedgerShowJson {
    schema_version: &'static str,
    command: &'static str,
    session: LedgerShow,
}

#[derive(Serialize)]
struct LedgerGetJson {
    schema_version: &'static str,
    command: &'static str,
    session_id: String,
    entry: LedgerEntryPayload,
}

#[derive(Serialize)]
struct LedgerVerdictJson {
    schema_version: &'static str,
    command: &'static str,
    verdict: LedgerVerdictBundle,
}

#[derive(Serialize)]
struct LedgerRecallJson {
    schema_version: &'static str,
    command: &'static str,
    recall: LedgerRecall,
}

#[derive(Serialize)]
struct LedgerVerifyJson {
    schema_version: &'static str,
    command: &'static str,
    verify: LedgerVerifyReport,
}

#[derive(Serialize)]
struct ReviewRecordedJson {
    schema_version: &'static str,
    command: &'static str,
    session_id: String,
    seq: i64,
    entry_hash: String,
    decision: HumanReviewDecision,
    note: Option<String>,
    attested_head_hash: String,
    reviewer_key_id: String,
}

fn handle_review_command(args: &Args, review: &ReviewArgs, mut out: impl Write) -> Result<()> {
    let command = match &review.command {
        Some(command) => command,
        // `truss review` with no decision launches the interactive TUI.
        None => return launch_review_tui(args, review.db.as_deref()),
    };
    let (decision, decision_args) = match command {
        ReviewCommand::Approve(decision_args) => (HumanReviewDecision::Approved, decision_args),
        ReviewCommand::Reject(decision_args) => (HumanReviewDecision::Rejected, decision_args),
        ReviewCommand::NeedsChanges(decision_args) => {
            (HumanReviewDecision::NeedsChanges, decision_args)
        }
    };
    let db = decision_args.db.as_deref().unwrap_or(&args.db);
    let key = decision_args.key.as_deref().unwrap_or(&args.key);
    let signer = Arc::new(Signer::load_or_create(key).map_err(|e| anyhow!(e.to_string()))?);
    let store = Store::open(db, signer).map_err(|e| anyhow!(e.to_string()))?;
    let entry = store
        .record_human_review(&decision_args.session, decision, decision_args.note.clone())
        .map_err(|e| anyhow!(e.to_string()))?;
    let payload: HumanReviewEntry =
        serde_json::from_value(entry.payload).map_err(|e| anyhow!(e.to_string()))?;
    let recorded = ReviewRecordedJson {
        schema_version: LEDGER_SCHEMA_VERSION,
        command: "review",
        session_id: decision_args.session.clone(),
        seq: entry.seq,
        entry_hash: hex::encode(entry.entry_hash),
        decision: payload.decision,
        note: payload.note,
        attested_head_hash: payload.attested_head_hash,
        reviewer_key_id: payload.reviewer_key_id,
    };

    if decision_args.json {
        serde_json::to_writer_pretty(&mut out, &recorded)?;
        writeln!(out)?;
    } else {
        writeln!(out, "session: {}", recorded.session_id)?;
        writeln!(
            out,
            "decision: {}",
            human_review_decision_label(recorded.decision)
        )?;
        if let Some(note) = &recorded.note {
            writeln!(out, "note: {note}")?;
        }
        writeln!(out, "attested_head_hash: {}", recorded.attested_head_hash)?;
        writeln!(out, "reviewer_key_id: {}", recorded.reviewer_key_id)?;
        writeln!(out, "entry: {}#{}", recorded.entry_hash, recorded.seq)?;
    }
    Ok(())
}

/// Launch the interactive review TUI over the durable store.
///
/// When the binary is built without the `tui` feature this is a clear,
/// non-panicking error telling the user to rebuild — the non-interactive
/// `truss review approve|reject|needs-changes` forms remain available.
#[cfg(feature = "tui")]
fn launch_review_tui(args: &Args, db_override: Option<&str>) -> Result<()> {
    let db = db_override.unwrap_or(&args.db);
    let signer = Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
    let store = Store::open(db, signer).map_err(|e| anyhow!(e.to_string()))?;
    tui::run(&store, db.to_string())
}

#[cfg(not(feature = "tui"))]
fn launch_review_tui(_args: &Args, _db_override: Option<&str>) -> Result<()> {
    Err(anyhow!(
        "the interactive review TUI is not compiled in; rebuild with `--features tui` \
         (e.g. `cargo build -p truss-cli --features tui`). The non-interactive \
         `truss review approve|reject|needs-changes <session>` commands work without it."
    ))
}

fn handle_ledger_command(args: &Args, ledger: &LedgerArgs, mut out: impl Write) -> Result<()> {
    let store = Store::open_readonly(&args.db).map_err(|e| anyhow!(e.to_string()))?;
    let reader = LedgerReader::new(store);
    match &ledger.command {
        LedgerCommand::List { json } => {
            let sessions = reader.list_sessions().map_err(|e| anyhow!(e.to_string()))?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &LedgerListJson {
                        schema_version: LEDGER_SCHEMA_VERSION,
                        command: "list",
                        sessions,
                    },
                )?;
                writeln!(out)?;
            } else {
                writeln!(
                    out,
                    "{:<24} {:>13} {:<12} {:<12} {:<12} HEAD",
                    "SESSION", "STARTED_MS", "ENTRIES", "VERIFY", "VERDICT"
                )?;
                for session in sessions {
                    writeln!(
                        out,
                        "{:<24} {:>13} {:<12} {:<12} {:<12} {}",
                        session.id,
                        session.started_at_ms,
                        session.entry_count,
                        format!("{:?}", session.verification.status).to_ascii_lowercase(),
                        session.verdict_status.as_deref().unwrap_or("unknown"),
                        session.head_hash.as_deref().map(short_hash).unwrap_or("-")
                    )?;
                }
            }
        }
        LedgerCommand::Show { run_id, json } => {
            let session = reader.show(run_id).map_err(|e| anyhow!(e.to_string()))?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &LedgerShowJson {
                        schema_version: LEDGER_SCHEMA_VERSION,
                        command: "show",
                        session,
                    },
                )?;
                writeln!(out)?;
            } else {
                writeln!(out, "session: {}", session.session_id)?;
                writeln!(out, "head: {}", session.head_hash.as_deref().unwrap_or("-"))?;
                writeln!(
                    out,
                    "verify: {}{}",
                    format!("{:?}", session.verification.status).to_ascii_lowercase(),
                    session
                        .verification
                        .failed_at_seq
                        .map(|seq| format!(" (failed at seq {seq})"))
                        .unwrap_or_default()
                )?;
                writeln!(
                    out,
                    "{:<20} {:>5} {:<12} {:<8} {:<12} SUMMARY",
                    "KIND", "SEQ", "HASH", "SIGNED", "VERIFY"
                )?;
                for entry in session.entries {
                    writeln!(
                        out,
                        "{:<20} {:>5} {:<12} {:<8} {:<12} {}",
                        entry.kind,
                        entry.seq,
                        entry.short_hash,
                        if entry.signed { "yes" } else { "no" },
                        format!("{:?}", entry.verification_status).to_ascii_lowercase(),
                        entry.summary
                    )?;
                }
            }
        }
        LedgerCommand::Get { run_id, seq, json } => {
            let entry = reader
                .get(run_id, *seq)
                .map_err(|e| anyhow!(e.to_string()))?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &LedgerGetJson {
                        schema_version: LEDGER_SCHEMA_VERSION,
                        command: "get",
                        session_id: run_id.clone(),
                        entry,
                    },
                )?;
                writeln!(out)?;
            } else {
                writeln!(out, "session: {run_id}")?;
                writeln!(out, "seq: {}", entry.seq)?;
                writeln!(out, "kind: {}", entry.kind)?;
                writeln!(out, "hash: {}", entry.hash)?;
                writeln!(out, "signed: {}", if entry.signed { "yes" } else { "no" })?;
                writeln!(
                    out,
                    "verify: {}",
                    format!("{:?}", entry.verification_status).to_ascii_lowercase()
                )?;
                writeln!(out, "payload:")?;
                writeln!(out, "{}", serde_json::to_string_pretty(&entry.payload)?)?;
            }
        }
        LedgerCommand::Verdict { run_id, json } => {
            let verdict = reader.verdict(run_id).map_err(|e| anyhow!(e.to_string()))?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &LedgerVerdictJson {
                        schema_version: LEDGER_SCHEMA_VERSION,
                        command: "verdict",
                        verdict,
                    },
                )?;
                writeln!(out)?;
            } else {
                render_ledger_verdict(&verdict, &mut out)?;
            }
        }
        LedgerCommand::Export { run_id } => {
            let export = reader.export(run_id).map_err(|e| anyhow!(e.to_string()))?;
            serde_json::to_writer_pretty(&mut out, &export)?;
            writeln!(out)?;
        }
        LedgerCommand::Recall {
            run_id,
            summary,
            json,
        } => {
            let recall = reader
                .recall(run_id, summary.as_deref())
                .map_err(|e| anyhow!(e.to_string()))?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &LedgerRecallJson {
                        schema_version: LEDGER_SCHEMA_VERSION,
                        command: "recall",
                        recall,
                    },
                )?;
                writeln!(out)?;
            } else {
                render_ledger_recall(&recall, &mut out)?;
            }
        }
        LedgerCommand::Verify { run_id, json } => {
            let verify = reader
                .verify(run_id, args.expect_key.as_deref())
                .map_err(|e| anyhow!(e.to_string()))?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &LedgerVerifyJson {
                        schema_version: LEDGER_SCHEMA_VERSION,
                        command: "verify",
                        verify,
                    },
                )?;
                writeln!(out)?;
            } else {
                render_ledger_verify(&verify, &mut out)?;
            }
        }
    }
    Ok(())
}

fn short_hash(hash: &str) -> &str {
    &hash[..12.min(hash.len())]
}

fn human_review_decision_label(decision: HumanReviewDecision) -> &'static str {
    match decision {
        HumanReviewDecision::Approved => "approved",
        HumanReviewDecision::Rejected => "rejected",
        HumanReviewDecision::NeedsChanges => "needs_changes",
    }
}

fn render_ledger_verdict(verdict: &LedgerVerdictBundle, out: &mut impl Write) -> Result<()> {
    writeln!(out, "session: {}", verdict.session_id)?;
    writeln!(
        out,
        "verify: {}{}",
        format!("{:?}", verdict.verification.status).to_ascii_lowercase(),
        verdict
            .verification
            .failed_at_seq
            .map(|seq| format!(" (failed at seq {seq})"))
            .unwrap_or_default()
    )?;
    writeln!(out, "head: {}", verdict.head_hash.as_deref().unwrap_or("-"))?;
    writeln!(out, "overall: {}", verdict.overall)?;
    if let Some(recorded) = &verdict.recorded_overall {
        if recorded != &verdict.overall {
            writeln!(out, "recorded_overall: {recorded} (untrusted)")?;
        }
    }
    if let Some(signoff) = &verdict.human_signoff {
        writeln!(out, "\nhuman_signoff:")?;
        writeln!(
            out,
            "  decision: {}",
            human_review_decision_label(signoff.decision)
        )?;
        writeln!(out, "  attested_head_hash: {}", signoff.attested_head_hash)?;
        writeln!(out, "  reviewer_key_id: {}", signoff.reviewer_key_id)?;
        if let Some(note) = &signoff.note {
            writeln!(out, "  note: {note}")?;
        }
        writeln!(
            out,
            "  attested_head_matches_current: {}",
            signoff.attested_head_matches_current
        )?;
        if let Some(current) = &signoff.current_reviewable_head_hash {
            writeln!(out, "  current_reviewable_head_hash: {current}")?;
        }
    }
    if let Some(contract) = &verdict.contract {
        writeln!(
            out,
            "\ncontract: {} — {}",
            contract.id.as_deref().unwrap_or("unknown"),
            contract.title.as_deref().unwrap_or("untitled")
        )?;
        if let Some(intent) = &contract.intent {
            writeln!(out, "intent: {intent}")?;
        }
        if !contract.definition_of_done.is_empty() {
            writeln!(out, "definition_of_done:")?;
            for clause in &contract.definition_of_done {
                writeln!(out, "  - [{}] {}", clause.kind, clause.desc)?;
            }
        }
    }
    if !verdict.evidence.checks.is_empty() {
        writeln!(out, "\nchecks:")?;
        for check in &verdict.evidence.checks {
            writeln!(out, "  {} [{}] {}", check.verdict, check.kind, check.desc)?;
            if let Some(first) = check.evidence.lines().next() {
                writeln!(out, "    {first}")?;
            }
        }
    }
    if !verdict.evidence.self_validation.is_empty() {
        writeln!(out, "\nself_validation:")?;
        for item in &verdict.evidence.self_validation {
            writeln!(
                out,
                "  exit={} cmd={}",
                item.exit
                    .map(|exit| exit.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                item.cmd.as_deref().unwrap_or("unknown")
            )?;
        }
    }
    if !verdict.evidence.panel.is_empty() {
        writeln!(out, "\npanel:")?;
        for panel in &verdict.evidence.panel {
            writeln!(out, "  {} {}", panel.clause_verdict, panel.desc)?;
            if let Some(majority) = &panel.majority {
                writeln!(
                    out,
                    "    majority: {} ({}/{})",
                    majority.decision, majority.met_count, majority.reviewer_count
                )?;
            }
            for reviewer in &panel.reviewers {
                writeln!(
                    out,
                    "    [{}] {} {}: {}",
                    reviewer.seat, reviewer.lens, reviewer.verdict, reviewer.reasoning
                )?;
            }
        }
    }
    if !verdict.anchor_refs.is_empty() {
        writeln!(out, "\nanchors:")?;
        for anchor in &verdict.anchor_refs {
            writeln!(
                out,
                "  {} #{} head {}",
                anchor.log,
                anchor.log_index,
                short_hash(&anchor.head_hash)
            )?;
        }
    }
    Ok(())
}

fn render_ledger_recall(recall: &LedgerRecall, out: &mut impl Write) -> Result<()> {
    writeln!(out, "session: {}", recall.session_id)?;
    writeln!(out, "head: {}", recall.head_hash.as_deref().unwrap_or("-"))?;
    writeln!(
        out,
        "verify: {}{}",
        format!("{:?}", recall.verification.status).to_ascii_lowercase(),
        recall
            .verification
            .failed_at_seq
            .map(|seq| format!(" (failed at seq {seq})"))
            .unwrap_or_default()
    )?;
    writeln!(out, "\nmemory:")?;
    if recall.memory_entries.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for memory in &recall.memory_entries {
        writeln!(
            out,
            "  {} {} trust={} verify={} produced={}#{}",
            memory.kind_label(),
            memory.id,
            format!("{:?}", memory.trust).to_ascii_lowercase(),
            format!("{:?}", memory.verification_status).to_ascii_lowercase(),
            memory.produced_by.session,
            memory.produced_by.seq
        )?;
        if let Some(key) = &memory.key {
            writeln!(out, "    key: {key}")?;
        }
        writeln!(out, "    {}", first_line(&memory.body))?;
    }
    writeln!(out, "\ncompactions:")?;
    if recall.compactions.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for compaction in &recall.compactions {
        let range = compaction
            .elided_range
            .as_ref()
            .map(|range| {
                format!(
                    "{}#{}..{}#{}",
                    range.start.session, range.start.seq, range.end.session, range.end.seq
                )
            })
            .unwrap_or_else(|| "-".into());
        writeln!(
            out,
            "  summary={} mode={} range={} entries={} recoverable={} verify={}",
            compaction.summary_memory_id.as_deref().unwrap_or("-"),
            compaction.mode.as_deref().unwrap_or("-"),
            range,
            compaction.original_entry_count,
            compaction.recoverable,
            format!("{:?}", compaction.entry.verification_status).to_ascii_lowercase()
        )?;
    }
    if let Some(expanded) = &recall.expanded_summary {
        writeln!(
            out,
            "\nexpanded_summary: {} ({} entries)",
            expanded.summary_memory_id,
            expanded.entries.len()
        )?;
        for entry in &expanded.entries {
            writeln!(
                out,
                "  #{:<4} {:<20} verify={} {}",
                entry.seq,
                entry.kind,
                format!("{:?}", entry.verification_status).to_ascii_lowercase(),
                serde_json::to_string(&entry.payload)?
            )?;
        }
    }
    Ok(())
}

trait MemoryKindLabel {
    fn kind_label(&self) -> &'static str;
}

impl MemoryKindLabel for truss_core::ledger::LedgerMemoryEntryView {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            truss_core::memory::EntryKind::Goal => "goal",
            truss_core::memory::EntryKind::Decision => "decision",
            truss_core::memory::EntryKind::Fact => "fact",
            truss_core::memory::EntryKind::Lesson => "lesson",
            truss_core::memory::EntryKind::Playbook => "playbook",
            truss_core::memory::EntryKind::Artifact => "artifact",
            truss_core::memory::EntryKind::Scratch => "scratch",
            truss_core::memory::EntryKind::ContextSummary => "context_summary",
        }
    }
}

fn render_ledger_verify(verify: &LedgerVerifyReport, out: &mut impl Write) -> Result<()> {
    writeln!(out, "session: {}", verify.session_id)?;
    writeln!(out, "head: {}", verify.head_hash.as_deref().unwrap_or("-"))?;
    match verify.verification.status {
        VerificationStatus::Verified => {
            writeln!(
                out,
                "OK: {} journal entries, hash chain + signatures + child anchors intact",
                verify.verification.verified_entries
            )?;
        }
        VerificationStatus::Unverified => {
            writeln!(
                out,
                "TAMPER: verification failed at seq {}",
                verify
                    .verification
                    .failed_at_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "unknown".into())
            )?;
        }
    }
    if !verify.entries.is_empty() {
        writeln!(out, "{:<20} {:>5} {:<12} VERIFY", "KIND", "SEQ", "HASH")?;
    }
    for entry in &verify.entries {
        writeln!(
            out,
            "{:<20} {:>5} {:<12} {}",
            entry.kind,
            entry.seq,
            entry.short_hash,
            format!("{:?}", entry.verification_status).to_ascii_lowercase()
        )?;
    }
    Ok(())
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn trust_label(trust: Trust) -> &'static str {
    match trust {
        Trust::Unverified => "unverified",
        Trust::Verified => "verified",
        Trust::Rejected => "rejected",
        Trust::Stale => "stale",
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PlaybookDoc {
    schema_version: u32,
    title: String,
    purpose: String,
    trigger: String,
    prerequisites: Vec<String>,
    procedure: Vec<String>,
    checks: Vec<String>,
    examples: Vec<String>,
    known_failures: Vec<String>,
    source_path: String,
}

#[derive(Debug, serde::Serialize)]
struct PlaybookEntryView {
    id: String,
    key: Option<String>,
    trust: Trust,
    title: String,
    purpose: String,
    produced_by: LedgerRef,
    verified_by: Option<LedgerRef>,
}

#[derive(Debug, serde::Serialize)]
struct PlaybookImportJson {
    playbook: PlaybookEntryView,
    import_session: String,
}

#[derive(Debug, serde::Serialize)]
struct PlaybookListJson {
    playbooks: Vec<PlaybookEntryView>,
}

#[derive(Debug, serde::Serialize)]
struct PlaybookShowJson {
    playbook: PlaybookEntryView,
    document: PlaybookDoc,
}

#[derive(Debug, serde::Serialize)]
struct PlaybookPromoteJson {
    playbook: PlaybookEntryView,
    promote_session: String,
    promoted: bool,
    event_kind: String,
    verdict_ref: LedgerRef,
    verdict_overall: delegation::ClauseVerdict,
    verdict_evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    eval_report_ref: Option<LedgerRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    eval_report: Option<truss_core::eval::EvalReport>,
}

#[derive(Debug, serde::Serialize)]
struct PlaybookRevokeJson {
    playbook: PlaybookEntryView,
    revoke_session: String,
}

struct PlaybookPromotionOutcome {
    entry: truss_core::memory::MemoryEntry,
    session_id: String,
    promoted: bool,
    event_kind: String,
    verdict_ref: LedgerRef,
    verdict: delegation::Verdict,
    eval_report: Option<truss_core::eval::EvalReport>,
}

fn handle_playbook_command(
    args: &Args,
    command: &PlaybookCommand,
    mut out: impl Write,
) -> Result<()> {
    let signer = Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
    let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
    let memory = SqliteMemoryStore::from_store(&store).map_err(|e| anyhow!(e.to_string()))?;

    match command {
        PlaybookCommand::Import { path, key, json } => {
            let source = std::fs::read_to_string(path)?;
            let key = key.clone().unwrap_or_else(|| key_from_path(path));
            let doc = parse_playbook_doc(path, &source);
            let body = serde_json::to_string_pretty(&doc)?;
            let session_id = unique_session_id("playbook-import");
            store
                .create_session(&session_id, Some("playbook-import"), "local", "local")
                .map_err(|e| anyhow!(e.to_string()))?;
            let entry = store
                .append(
                    &session_id,
                    "playbook_imported",
                    &serde_json::json!({
                        "path": path,
                        "key": key,
                        "schema_version": doc.schema_version,
                        "title": doc.title,
                    }),
                )
                .map_err(|e| anyhow!(e.to_string()))?;
            let produced_by = LedgerRef {
                session: session_id.clone(),
                seq: entry.seq,
            };
            let id = memory
                .write(MemoryDraft {
                    kind: EntryKind::Playbook,
                    key: Some(key),
                    body,
                    produced_by: produced_by.clone(),
                    sources: vec![produced_by],
                })
                .map_err(|e| anyhow!(e.to_string()))?;
            store
                .set_status(&session_id, "completed")
                .map_err(|e| anyhow!(e.to_string()))?;
            let entry = memory.get(&id).map_err(|e| anyhow!(e.to_string()))?;
            let view = playbook_entry_view(&entry)?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &PlaybookImportJson {
                        playbook: view,
                        import_session: session_id,
                    },
                )?;
                writeln!(out)?;
            } else {
                writeln!(out, "imported playbook: {}", view.id)?;
                writeln!(out, "key: {}", view.key.as_deref().unwrap_or("-"))?;
                writeln!(out, "trust: {}", trust_label(view.trust))?;
                writeln!(out, "title: {}", view.title)?;
                writeln!(
                    out,
                    "provenance: {}#{}",
                    view.produced_by.session, view.produced_by.seq
                )?;
            }
        }
        PlaybookCommand::List {
            verified_only,
            json,
        } => {
            let entries = memory
                .query(&MemoryQuery {
                    kind: Some(EntryKind::Playbook),
                    min_trust: if *verified_only {
                        Trust::Verified
                    } else {
                        Trust::Unverified
                    },
                    limit: 100,
                    ..MemoryQuery::default()
                })
                .map_err(|e| anyhow!(e.to_string()))?;
            let views = entries
                .iter()
                .map(playbook_entry_view)
                .collect::<Result<Vec<_>>>()?;
            if *json {
                serde_json::to_writer_pretty(&mut out, &PlaybookListJson { playbooks: views })?;
                writeln!(out)?;
            } else {
                writeln!(out, "{:<12} {:<64} KEY", "TRUST", "ID")?;
                for view in views {
                    writeln!(
                        out,
                        "{:<12} {:<64} {}",
                        trust_label(view.trust),
                        view.id,
                        view.key.as_deref().unwrap_or("-")
                    )?;
                }
            }
        }
        PlaybookCommand::Show { id_or_key, json } => {
            let entry = find_playbook(&memory, id_or_key)?;
            let view = playbook_entry_view(&entry)?;
            let doc = playbook_doc_from_entry(&entry)?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &PlaybookShowJson {
                        playbook: view,
                        document: doc,
                    },
                )?;
                writeln!(out)?;
            } else {
                render_playbook(&view, &doc, &mut out)?;
            }
        }
        PlaybookCommand::Promote { .. } => {
            return Err(anyhow!(
                "playbook promote requires the async promotion handler"
            ));
        }
        PlaybookCommand::Revoke {
            id_or_key,
            reason,
            json,
        } => {
            let entry = find_playbook(&memory, id_or_key)?;
            let doc = playbook_doc_from_entry(&entry)?;
            let session_id = unique_session_id("playbook-revoke");
            store
                .create_session(&session_id, Some("playbook-revoke"), "local", "local")
                .map_err(|e| anyhow!(e.to_string()))?;
            store
                .append(
                    &session_id,
                    "playbook_revoked",
                    &serde_json::json!({
                        "id": entry.id.0,
                        "key": entry.key,
                        "title": doc.title,
                        "reason": reason.as_deref().unwrap_or("operator revoked"),
                    }),
                )
                .map_err(|e| anyhow!(e.to_string()))?;
            memory
                .mark_stale(&entry.id, None)
                .map_err(|e| anyhow!(e.to_string()))?;
            store
                .set_status(&session_id, "completed")
                .map_err(|e| anyhow!(e.to_string()))?;
            let entry = memory.get(&entry.id).map_err(|e| anyhow!(e.to_string()))?;
            let view = playbook_entry_view(&entry)?;
            if *json {
                serde_json::to_writer_pretty(
                    &mut out,
                    &PlaybookRevokeJson {
                        playbook: view,
                        revoke_session: session_id,
                    },
                )?;
                writeln!(out)?;
            } else {
                writeln!(out, "revoked playbook: {}", view.id)?;
                writeln!(out, "trust: {}", trust_label(view.trust))?;
            }
        }
    }
    Ok(())
}

async fn handle_playbook_promote_command(
    args: &Args,
    id_or_key: &str,
    json: bool,
    reviewers: Vec<Arc<dyn Provider>>,
    ctx: &Ctx,
    mut out: impl Write,
) -> Result<bool> {
    let signer = Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
    let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
    let memory = SqliteMemoryStore::from_store(&store).map_err(|e| anyhow!(e.to_string()))?;
    let entry = find_playbook(&memory, id_or_key)?;
    let outcome = promote_playbook_entry(&memory, &store, &entry, &reviewers, ctx).await?;
    let view = playbook_entry_view(&outcome.entry)?;
    let verdict_evidence = outcome
        .verdict
        .results
        .first()
        .map(|result| result.evidence.clone())
        .unwrap_or_default();
    if json {
        serde_json::to_writer_pretty(
            &mut out,
            &PlaybookPromoteJson {
                playbook: view,
                promote_session: outcome.session_id,
                promoted: outcome.promoted,
                event_kind: outcome.event_kind,
                verdict_ref: outcome.verdict_ref,
                verdict_overall: outcome.verdict.overall,
                verdict_evidence,
                eval_report_ref: outcome
                    .eval_report
                    .as_ref()
                    .and_then(|report| report.report_ref.clone()),
                eval_report: outcome.eval_report,
            },
        )?;
        writeln!(out)?;
    } else if outcome.promoted {
        writeln!(out, "promoted playbook: {}", view.id)?;
        writeln!(out, "key: {}", view.key.as_deref().unwrap_or("-"))?;
        writeln!(out, "trust: {}", trust_label(view.trust))?;
        writeln!(out, "verdict: {:?}", outcome.verdict.overall)?;
        writeln!(
            out,
            "verified_by: {}#{}",
            outcome.verdict_ref.session, outcome.verdict_ref.seq
        )?;
    } else {
        writeln!(out, "playbook promotion rejected: {}", view.id)?;
        writeln!(out, "key: {}", view.key.as_deref().unwrap_or("-"))?;
        writeln!(out, "trust: {}", trust_label(view.trust))?;
        writeln!(out, "verdict: {:?}", outcome.verdict.overall)?;
        writeln!(
            out,
            "rejection_event: {}#{}",
            outcome.verdict_ref.session, outcome.verdict_ref.seq
        )?;
    }
    Ok(outcome.promoted)
}

async fn promote_playbook_entry(
    memory: &SqliteMemoryStore,
    store: &Store,
    entry: &truss_core::memory::MemoryEntry,
    reviewers: &[Arc<dyn Provider>],
    ctx: &Ctx,
) -> Result<PlaybookPromotionOutcome> {
    if entry.trust != Trust::Unverified {
        return Err(anyhow!(
            "playbook {} is {}; only unverified drafts can be promoted",
            entry.id.0,
            trust_label(entry.trust)
        ));
    }
    let doc = playbook_doc_from_entry(entry)?;
    let evidence = playbook_promotion_evidence(&doc, entry);
    let verdict =
        delegation::run_memory_claim_with_evidence(reviewers, ctx, entry, Some(&evidence)).await;
    let promoted = verdict.overall == delegation::ClauseVerdict::Pass;
    let event_kind = if promoted {
        "playbook_promoted"
    } else {
        "playbook_promotion_rejected"
    };
    let session_id = unique_session_id("playbook-promote");
    store
        .create_session(&session_id, Some("playbook-promote"), "local", "local")
        .map_err(|e| anyhow!(e.to_string()))?;
    let appended = store
        .append(
            &session_id,
            event_kind,
            &serde_json::json!({
                "id": entry.id.0,
                "key": entry.key,
                "title": doc.title,
                "overall": format!("{:?}", verdict.overall),
                "verdict": verdict.clone(),
            }),
        )
        .map_err(|e| anyhow!(e.to_string()))?;
    let verdict_ref = LedgerRef {
        session: session_id.clone(),
        seq: appended.seq,
    };
    if promoted {
        memory
            .promote_with_verdict_ref(&entry.id, verdict.clone(), verdict_ref.clone())
            .map_err(|e| anyhow!(e.to_string()))?;
        store
            .set_status(&session_id, "completed")
            .map_err(|e| anyhow!(e.to_string()))?;
    } else {
        store
            .set_status(&session_id, "failed")
            .map_err(|e| anyhow!(e.to_string()))?;
    }
    let entry = memory.get(&entry.id).map_err(|e| anyhow!(e.to_string()))?;
    Ok(PlaybookPromotionOutcome {
        entry,
        session_id,
        promoted,
        event_kind: event_kind.into(),
        verdict_ref,
        verdict,
        eval_report: None,
    })
}

struct PlaybookEvalPromoteOptions {
    contract: Arc<Contract>,
    repeats: usize,
    budget_slice: truss_core::budget::BudgetLimits,
    base_cfg: DurableConfig,
    tools: Arc<ToolRegistry>,
    provider: Arc<dyn Provider>,
    reviewers: Vec<Arc<dyn Provider>>,
    ctx: Ctx,
}

async fn handle_playbook_eval_promote_command(
    args: &Args,
    id_or_key: &str,
    json: bool,
    options: PlaybookEvalPromoteOptions,
    mut out: impl Write,
) -> Result<bool> {
    let signer = Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
    let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
    let memory =
        Arc::new(SqliteMemoryStore::from_store(&store).map_err(|e| anyhow!(e.to_string()))?);
    let entry = find_playbook(memory.as_ref(), id_or_key)?;
    let outcome = promote_playbook_entry_with_eval(memory, &store, &entry, options).await?;
    let view = playbook_entry_view(&outcome.entry)?;
    let verdict_evidence = outcome
        .verdict
        .results
        .first()
        .map(|result| result.evidence.clone())
        .unwrap_or_default();
    if json {
        serde_json::to_writer_pretty(
            &mut out,
            &PlaybookPromoteJson {
                playbook: view,
                promote_session: outcome.session_id,
                promoted: outcome.promoted,
                event_kind: outcome.event_kind,
                verdict_ref: outcome.verdict_ref,
                verdict_overall: outcome.verdict.overall,
                verdict_evidence,
                eval_report_ref: outcome
                    .eval_report
                    .as_ref()
                    .and_then(|report| report.report_ref.clone()),
                eval_report: outcome.eval_report,
            },
        )?;
        writeln!(out)?;
    } else if outcome.promoted {
        writeln!(out, "promoted playbook: {}", view.id)?;
        writeln!(out, "key: {}", view.key.as_deref().unwrap_or("-"))?;
        writeln!(out, "trust: {}", trust_label(view.trust))?;
        writeln!(out, "verdict: {:?}", outcome.verdict.overall)?;
        writeln!(
            out,
            "eval_report: {}#{}",
            outcome
                .eval_report
                .as_ref()
                .and_then(|report| report.report_ref.as_ref())
                .map(|ledger| ledger.session.as_str())
                .unwrap_or("-"),
            outcome
                .eval_report
                .as_ref()
                .and_then(|report| report.report_ref.as_ref())
                .map(|ledger| ledger.seq)
                .unwrap_or(-1)
        )?;
        writeln!(
            out,
            "verified_by: {}#{}",
            outcome.verdict_ref.session, outcome.verdict_ref.seq
        )?;
    } else {
        writeln!(out, "playbook promotion rejected: {}", view.id)?;
        writeln!(out, "key: {}", view.key.as_deref().unwrap_or("-"))?;
        writeln!(out, "trust: {}", trust_label(view.trust))?;
        writeln!(out, "verdict: {:?}", outcome.verdict.overall)?;
        writeln!(
            out,
            "rejection_event: {}#{}",
            outcome.verdict_ref.session, outcome.verdict_ref.seq
        )?;
    }
    Ok(outcome.promoted)
}

async fn promote_playbook_entry_with_eval(
    memory: Arc<SqliteMemoryStore>,
    store: &Store,
    entry: &truss_core::memory::MemoryEntry,
    mut options: PlaybookEvalPromoteOptions,
) -> Result<PlaybookPromotionOutcome> {
    if entry.trust != Trust::Unverified {
        return Err(anyhow!(
            "playbook {} is {}; only unverified drafts can be promoted",
            entry.id.0,
            trust_label(entry.trust)
        ));
    }
    if options.repeats == 0 {
        return Err(anyhow!(
            "playbook eval promotion requires at least one repeat"
        ));
    }

    let doc = playbook_doc_from_entry(entry)?;
    let eval_memory: Arc<dyn MemoryStore> = Arc::new(PlaybookEvalMemoryStore {
        inner: memory.clone(),
        candidate: entry.clone(),
    });
    options.base_cfg.memory_store = Some(eval_memory.clone());
    let report = truss_core::eval::run_paired_eval(
        &options.ctx,
        store,
        options.contract.clone(),
        truss_core::eval::PairedEvalOptions {
            repeats: options.repeats,
            budget_slice: options.budget_slice,
            base_cfg: options.base_cfg,
            tools: options.tools,
            provider: options.provider,
            reviewers: options.reviewers,
            memory_store: eval_memory,
        },
    )
    .await
    .map_err(|e| anyhow!(e.to_string()))?;

    let promoted = report.delta.pass_rate > 0.0;
    let verdict = playbook_eval_verdict(&doc, entry, &report, promoted)?;
    let event_kind = if promoted {
        "playbook_promoted"
    } else {
        "playbook_promotion_rejected"
    };
    let session_id = unique_session_id("playbook-promote");
    store
        .create_session(&session_id, Some("playbook-eval"), "local", "local")
        .map_err(|e| anyhow!(e.to_string()))?;
    let appended = store
        .append(
            &session_id,
            event_kind,
            &serde_json::json!({
                "id": entry.id.0,
                "key": entry.key,
                "title": doc.title,
                "overall": format!("{:?}", verdict.overall),
                "verdict": verdict.clone(),
                "eval_report": report.clone(),
                "eval_report_ref": report.report_ref.clone(),
                "minimum_pass_rate_delta": 0.0,
            }),
        )
        .map_err(|e| anyhow!(e.to_string()))?;
    let verdict_ref = LedgerRef {
        session: session_id.clone(),
        seq: appended.seq,
    };
    if promoted {
        memory
            .promote_with_verdict_ref(&entry.id, verdict.clone(), verdict_ref.clone())
            .map_err(|e| anyhow!(e.to_string()))?;
        store
            .set_status(&session_id, "completed")
            .map_err(|e| anyhow!(e.to_string()))?;
    } else {
        store
            .set_status(&session_id, "failed")
            .map_err(|e| anyhow!(e.to_string()))?;
    }
    let entry = memory.get(&entry.id).map_err(|e| anyhow!(e.to_string()))?;
    Ok(PlaybookPromotionOutcome {
        entry,
        session_id,
        promoted,
        event_kind: event_kind.into(),
        verdict_ref,
        verdict,
        eval_report: Some(report),
    })
}

fn playbook_eval_verdict(
    doc: &PlaybookDoc,
    entry: &truss_core::memory::MemoryEntry,
    report: &truss_core::eval::EvalReport,
    promoted: bool,
) -> Result<delegation::Verdict> {
    let evidence = serde_json::to_string_pretty(report).map_err(|e| anyhow!(e.to_string()))?;
    let overall = if promoted {
        delegation::ClauseVerdict::Pass
    } else {
        delegation::ClauseVerdict::Fail
    };
    let desc = if promoted {
        "paired eval showed positive pass-rate lift for candidate playbook"
    } else {
        "paired eval did not show positive pass-rate lift for candidate playbook"
    };
    Ok(delegation::Verdict {
        contract_id: report.contract_id.clone(),
        title: format!("Eval promotion for playbook {}", doc.title),
        overall: overall.clone(),
        results: vec![delegation::ClauseResult {
            kind: "eval".into(),
            desc: desc.into(),
            verdict: overall.clone(),
            evidence,
        }],
        executor_summary: format!(
            "Playbook {} ({}) eval delta pass_rate={:+.2}. {}",
            entry.id.0,
            entry.key.as_deref().unwrap_or("<no key>"),
            report.delta.pass_rate,
            report.summary
        ),
        reviewed_diff: None,
        usage: report.arms.iter().fold(
            truss_core::budget::BudgetSnapshot::default(),
            |mut acc, arm| {
                acc.input_tokens += arm.total_usage.input_tokens;
                acc.output_tokens += arm.total_usage.output_tokens;
                acc.cost_usd += arm.total_usage.cost_usd;
                acc.steps += arm.total_usage.steps;
                acc
            },
        ),
    })
}

struct PlaybookEvalMemoryStore {
    inner: Arc<SqliteMemoryStore>,
    candidate: MemoryEntry,
}

impl MemoryStore for PlaybookEvalMemoryStore {
    fn write(&self, draft: MemoryDraft) -> std::result::Result<MemoryId, MemoryError> {
        self.inner.write(draft)
    }

    fn promote(
        &self,
        id: &MemoryId,
        verdict: delegation::Verdict,
    ) -> std::result::Result<(), MemoryError> {
        self.inner.promote(id, verdict)
    }

    fn promote_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: delegation::Verdict,
        verdict_ref: LedgerRef,
    ) -> std::result::Result<(), MemoryError> {
        self.inner
            .promote_with_verdict_ref(id, verdict, verdict_ref)
    }

    fn reject(
        &self,
        id: &MemoryId,
        verdict: delegation::Verdict,
    ) -> std::result::Result<(), MemoryError> {
        self.inner.reject(id, verdict)
    }

    fn reject_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: delegation::Verdict,
        verdict_ref: LedgerRef,
    ) -> std::result::Result<(), MemoryError> {
        self.inner.reject_with_verdict_ref(id, verdict, verdict_ref)
    }

    fn mark_stale(
        &self,
        id: &MemoryId,
        superseded_by: Option<MemoryId>,
    ) -> std::result::Result<(), MemoryError> {
        self.inner.mark_stale(id, superseded_by)
    }

    fn get(&self, id: &MemoryId) -> std::result::Result<MemoryEntry, MemoryError> {
        self.inner.get(id)
    }

    fn query(&self, q: &MemoryQuery) -> std::result::Result<Vec<MemoryEntry>, MemoryError> {
        let mut entries = self.inner.query(q)?;
        if self.candidate_matches(q) && !entries.iter().any(|entry| entry.id == self.candidate.id) {
            let mut candidate = self.candidate.clone();
            candidate.trust = Trust::Verified;
            entries.insert(0, candidate);
            entries.truncate(q.limit);
        }
        Ok(entries)
    }

    fn delta_since(&self, cursor: MemoryCursor) -> std::result::Result<MemoryDelta, MemoryError> {
        self.inner.delta_since(cursor)
    }

    fn write_policy(&self, draft: PolicyDraft) -> std::result::Result<PolicyId, MemoryError> {
        self.inner.write_policy(draft)
    }

    fn get_policy(&self, id: &PolicyId) -> std::result::Result<PolicyRecord, MemoryError> {
        self.inner.get_policy(id)
    }
}

impl PlaybookEvalMemoryStore {
    fn candidate_matches(&self, q: &MemoryQuery) -> bool {
        if q.limit == 0 {
            return false;
        }
        if let Some(kind) = q.kind {
            if kind != EntryKind::Playbook {
                return false;
            }
        }
        if q.min_trust.rank() > Trust::Verified.rank() {
            return false;
        }
        if let Some(key) = &q.key {
            if self.candidate.key.as_ref() != Some(key) {
                return false;
            }
        }
        if let Some(contains) = &q.contains {
            if !self
                .candidate
                .body
                .to_ascii_lowercase()
                .contains(&contains.to_ascii_lowercase())
            {
                return false;
            }
        }
        true
    }
}

fn playbook_promotion_evidence(
    doc: &PlaybookDoc,
    entry: &truss_core::memory::MemoryEntry,
) -> String {
    format!(
        "Imported playbook draft.\nMemory id: {}\nKey: {}\nProduced by: {}#{}\n\
         Source path: {}\nTitle: {}\nPurpose:\n{}\n\nTrigger:\n{}\n\nPrerequisites:\n{}\n\n\
         Procedure:\n{}\n\nChecks:\n{}\n\nKnown failures:\n{}",
        entry.id.0,
        entry.key.as_deref().unwrap_or("<none>"),
        entry.provenance.produced_by.session,
        entry.provenance.produced_by.seq,
        doc.source_path,
        doc.title,
        doc.purpose,
        doc.trigger,
        doc.prerequisites.join("\n- "),
        doc.procedure.join("\n- "),
        doc.checks.join("\n- "),
        doc.known_failures.join("\n- ")
    )
}

fn playbook_reviewer_panel(args: &Args, config: Option<&Config>) -> Result<Vec<Arc<dyn Provider>>> {
    let provider = match config {
        Some(c) => c.build_provider()?,
        None => build_provider(args)?,
    };
    let reviewer = match &args.reviewer_model {
        Some(rm) => build_named_provider(args, rm)?,
        None => provider,
    };
    let panel_size = args.reviewers.unwrap_or(1).max(1);
    Ok((0..panel_size).map(|_| reviewer.clone()).collect())
}

fn parse_playbook_doc(path: &str, source: &str) -> PlaybookDoc {
    let title = first_heading(source).unwrap_or_else(|| key_from_path(path));
    let purpose = front_matter_field(source, "description")
        .or_else(|| section_text(source, &["purpose", "overview", "description"]))
        .or_else(|| first_paragraph(source))
        .unwrap_or_else(|| title.clone());
    PlaybookDoc {
        schema_version: 1,
        title,
        purpose,
        trigger: section_text(source, &["trigger", "when to use", "use when"]).unwrap_or_default(),
        prerequisites: section_items(source, &["prerequisites", "requirements", "setup"]),
        procedure: section_items(
            source,
            &["procedure", "workflow", "how to use", "instructions"],
        ),
        checks: section_items(source, &["checks", "verification", "tests", "validation"]),
        examples: section_items(source, &["examples", "example"]),
        known_failures: section_items(
            source,
            &["known failures", "pitfalls", "caveats", "failure modes"],
        ),
        source_path: path.to_string(),
    }
}

fn playbook_entry_view(entry: &truss_core::memory::MemoryEntry) -> Result<PlaybookEntryView> {
    let doc = playbook_doc_from_entry(entry)?;
    Ok(PlaybookEntryView {
        id: entry.id.0.clone(),
        key: entry.key.clone(),
        trust: entry.trust,
        title: doc.title,
        purpose: doc.purpose,
        produced_by: entry.provenance.produced_by.clone(),
        verified_by: entry.provenance.verified_by_ref.clone(),
    })
}

fn playbook_doc_from_entry(entry: &truss_core::memory::MemoryEntry) -> Result<PlaybookDoc> {
    serde_json::from_str(&entry.body).map_err(|e| anyhow!("invalid playbook body: {e}"))
}

fn find_playbook(
    memory: &SqliteMemoryStore,
    id_or_key: &str,
) -> Result<truss_core::memory::MemoryEntry> {
    if let Ok(entry) = memory.get(&MemoryId(id_or_key.to_string())) {
        if entry.kind == EntryKind::Playbook {
            return Ok(entry);
        }
    }
    let entries = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Playbook),
            key: Some(id_or_key.to_string()),
            min_trust: Trust::Unverified,
            limit: 1,
            ..MemoryQuery::default()
        })
        .map_err(|e| anyhow!(e.to_string()))?;
    entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("playbook not found: {id_or_key}"))
}

fn render_playbook(
    view: &PlaybookEntryView,
    doc: &PlaybookDoc,
    out: &mut impl Write,
) -> Result<()> {
    writeln!(out, "id: {}", view.id)?;
    writeln!(out, "key: {}", view.key.as_deref().unwrap_or("-"))?;
    writeln!(out, "trust: {}", trust_label(view.trust))?;
    writeln!(out, "title: {}", doc.title)?;
    writeln!(out, "purpose: {}", doc.purpose)?;
    if !doc.trigger.is_empty() {
        writeln!(out, "trigger: {}", doc.trigger)?;
    }
    render_list_section(out, "prerequisites", &doc.prerequisites)?;
    render_list_section(out, "procedure", &doc.procedure)?;
    render_list_section(out, "checks", &doc.checks)?;
    render_list_section(out, "examples", &doc.examples)?;
    render_list_section(out, "known_failures", &doc.known_failures)?;
    writeln!(
        out,
        "produced_by: {}#{}",
        view.produced_by.session, view.produced_by.seq
    )?;
    Ok(())
}

fn render_list_section(out: &mut impl Write, label: &str, items: &[String]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    writeln!(out, "{label}:")?;
    for item in items {
        writeln!(out, "  - {item}")?;
    }
    Ok(())
}

fn first_heading(source: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("# ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    })
}

fn first_paragraph(source: &str) -> Option<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| !line.starts_with('#') && !line.starts_with("---"))
        .map(clean_item)
}

fn front_matter_field(source: &str, field: &str) -> Option<String> {
    let mut lines = source.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        if key.trim() == field {
            return Some(value.trim().trim_matches('"').to_string()).filter(|s| !s.is_empty());
        }
    }
    None
}

fn section_text(source: &str, names: &[&str]) -> Option<String> {
    let items = section_items(source, names);
    (!items.is_empty()).then(|| items.join("\n"))
}

fn section_items(source: &str, names: &[&str]) -> Vec<String> {
    let mut in_section = false;
    let mut out = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(title) = heading_title(trimmed) {
            let normalized = normalize_heading(title);
            in_section = names
                .iter()
                .any(|name| normalized == *name || normalized.contains(name));
            continue;
        }
        if !in_section || trimmed.is_empty() || trimmed == "---" {
            continue;
        }
        out.push(clean_item(trimmed));
    }
    out
}

fn heading_title(line: &str) -> Option<&str> {
    let title = line.trim_start_matches('#');
    (title.len() != line.len())
        .then(|| title.trim())
        .filter(|title| !title.is_empty())
}

fn normalize_heading(title: &str) -> String {
    title
        .trim()
        .trim_matches(':')
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
}

fn clean_item(line: &str) -> String {
    line.trim()
        .trim_start_matches("- ")
        .trim_start_matches("* ")
        .trim_start_matches(char::is_numeric)
        .trim_start_matches(". ")
        .trim()
        .to_string()
}

fn key_from_path(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("playbook");
    slugify(stem)
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(|ch| ch.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "playbook".into()
    } else {
        out
    }
}

fn unique_session_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}", std::process::id())
}

#[tokio::main]
async fn main() -> Result<()> {
    truss_core::observability::init();
    let args = Args::parse();

    if let Some(Command::Ledger(ledger)) = &args.command {
        return handle_ledger_command(&args, ledger, std::io::stdout());
    }
    if let Some(Command::Review(review)) = &args.command {
        return handle_review_command(&args, review, std::io::stdout());
    }
    if let Some(Command::Tui(tui_args)) = &args.command {
        return launch_review_tui(&args, tui_args.db.as_deref());
    }
    if let Some(Command::Playbook(playbook)) = &args.command {
        if let PlaybookCommand::Promote {
            id_or_key,
            eval,
            contract,
            repeats,
            json,
        } = &playbook.command
        {
            let config = match &args.config {
                Some(path) => Some(Config::from_path(path)?),
                None => None,
            };
            if *eval {
                let contract_path = contract
                    .as_deref()
                    .ok_or_else(|| anyhow!("playbook promote --eval requires --contract <file>"))?;
                let repeats = repeats.unwrap_or(1);
                let contract = Arc::new(Contract::from_path(contract_path)?);
                let contract_memory_config =
                    resolved_memory_config(config.as_ref(), Some(&contract.memory), &args);
                let contract_approval_gates =
                    resolved_approval_gate_policy(config.as_ref(), Some(&contract.approval_gates));
                let mut contract_policy = contract.policy();
                let contract_sandbox_config =
                    resolved_sandbox_config(config.as_ref(), Some(&contract.sandbox));
                if contract_sandbox_config.mode == SandboxMode::WorkingCopy {
                    allow_self_validation_tools(&mut contract_policy);
                }
                let contract_sandbox = sandbox_from_config(
                    &contract_sandbox_config,
                    default_checkout(&args, &contract_policy),
                );
                let mut limits = config
                    .as_ref()
                    .map(|c| c.budget_limits())
                    .unwrap_or_default();
                if args.budget_usd.is_some() {
                    limits.max_cost_usd = args.budget_usd;
                }
                if args.budget_tokens.is_some() {
                    limits.max_total_tokens = args.budget_tokens;
                }
                let provider = match &config {
                    Some(c) => c.build_provider()?,
                    None => build_provider(&args)?,
                };
                let reviewer = match &args.reviewer_model {
                    Some(rm) => build_named_provider(&args, rm)?,
                    None => provider.clone(),
                };
                let panel_size = args.reviewers.or(contract.reviewers).unwrap_or(1).max(1);
                let reviewers: Vec<Arc<dyn Provider>> =
                    (0..panel_size).map(|_| reviewer.clone()).collect();
                let eff_max_tokens = args
                    .max_tokens
                    .or_else(|| config.as_ref().and_then(|c| c.agent.max_tokens))
                    .unwrap_or(4096);
                let eff_max_steps = args
                    .max_steps
                    .or_else(|| config.as_ref().and_then(|c| c.agent.max_steps))
                    .unwrap_or(12);
                let eff_reasoning: Option<ReasoningEffort> = args
                    .reasoning
                    .map(Into::into)
                    .or_else(|| config.as_ref().and_then(|c| c.agent.reasoning));
                let eff_temperature: Option<f32> = args
                    .temperature
                    .or_else(|| config.as_ref().and_then(|c| c.agent.temperature));
                let mut eval_ctx = Ctx::new()
                    .with_policy(contract_policy)
                    .with_sandbox(contract_sandbox)
                    .with_budget(BudgetMeter::new(limits));
                if let Some(secs) = args.timeout {
                    eval_ctx = eval_ctx.with_timeout(std::time::Duration::from_secs(secs));
                }
                let mut base_tools = if args.tools {
                    ToolRegistry::with_defaults()
                } else {
                    ToolRegistry::new()
                };
                register_native_tools_without_journal(&mut base_tools, &args, config.as_ref());
                register_web_tools_without_journal(&mut base_tools, &args, config.as_ref());
                register_memory_tools_without_journal(
                    &mut base_tools,
                    &args,
                    config.as_ref(),
                    &contract_memory_config,
                )?;
                let global_mcp_servers = mcp_servers_for(config.as_ref(), None);
                if !global_mcp_servers.is_empty() {
                    base_tools
                        .register_mcp_servers(&eval_ctx, &global_mcp_servers, None)
                        .await
                        .map_err(|e| anyhow!(e.to_string()))?;
                }
                let exec_cfg = DurableConfig {
                    system: contract.constraints.system.clone(),
                    model: args.model.clone(),
                    max_tokens: eff_max_tokens,
                    max_steps: contract
                        .budget
                        .max_steps
                        .map(|n| n as usize)
                        .unwrap_or(eff_max_steps),
                    retry: RetryPolicy::default(),
                    reasoning: eff_reasoning,
                    temperature: eff_temperature,
                    max_context_msgs: args
                        .max_context_msgs
                        .max(contract_memory_config.projection.keep_tail),
                    memory_tool: args.memory_tool,
                    memory: contract_memory_config,
                    persona: None,
                    principal: None,
                    goal_ledger: None,
                    memory_cursor: truss_core::memory::MemoryCursor(0),
                    memory_store: None,
                    approval_gates: contract_approval_gates,
                };
                let promoted = handle_playbook_eval_promote_command(
                    &args,
                    id_or_key,
                    *json,
                    PlaybookEvalPromoteOptions {
                        contract: contract.clone(),
                        repeats,
                        budget_slice: contract.budget_limits(),
                        base_cfg: exec_cfg,
                        tools: Arc::new(base_tools),
                        provider,
                        reviewers,
                        ctx: eval_ctx,
                    },
                    std::io::stdout(),
                )
                .await?;
                if !promoted {
                    std::process::exit(1);
                }
                return Ok(());
            }
            if contract.is_some() || repeats.is_some() {
                return Err(anyhow!(
                    "playbook promote --contract/--repeats are only valid with --eval"
                ));
            }
            let reviewers = playbook_reviewer_panel(&args, config.as_ref())?;
            let mut ctx = Ctx::new().with_policy(resolved_policy(&args, config.as_ref()));
            if let Some(secs) = args.timeout {
                ctx = ctx.with_timeout(std::time::Duration::from_secs(secs));
            }
            let promoted = handle_playbook_promote_command(
                &args,
                id_or_key,
                *json,
                reviewers,
                &ctx,
                std::io::stdout(),
            )
            .await?;
            if !promoted {
                std::process::exit(1);
            }
            return Ok(());
        }
        return handle_playbook_command(&args, &playbook.command, std::io::stdout());
    }
    // `--verify`: check a session's hash chain + signatures and exit.
    if let Some(sid) = &args.verify {
        let store = Store::open_readonly(&args.db).map_err(|e| anyhow!(e.to_string()))?;
        let verify = LedgerReader::new(store)
            .verify(sid, args.expect_key.as_deref())
            .map_err(|e| anyhow!(e.to_string()))?;
        render_ledger_verify(&verify, &mut std::io::stdout())?;
        if verify.verification.status == VerificationStatus::Unverified {
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--signal`: durably deliver a signal to a session and exit (no provider).
    if let Some(name) = &args.signal {
        let session = args
            .session
            .clone()
            .ok_or_else(|| anyhow!("--signal requires --session"))?;
        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
        store
            .add_signal(&session, name, args.payload.as_deref())
            .map_err(|e| anyhow!(e.to_string()))?;
        eprintln!("[signal] delivered '{name}' to session {session}");
        eprintln!(
            "resume with: truss --resume {session} --db {}  (or run --daemon)",
            args.db
        );
        return Ok(());
    }

    // `--verify-anchor-log`: check the transparency log's own hash chain.
    if args.verify_anchor_log {
        let log = FileTransparencyLog::new(args.anchor_log.clone());
        match log.verify().map_err(|e| anyhow!(e.to_string()))? {
            Ok(n) => {
                println!(
                    "OK: transparency log {} — {n} anchors, chain intact",
                    args.anchor_log
                );
                return Ok(());
            }
            Err(i) => {
                println!("TAMPER: transparency log broken at index {i}");
                std::process::exit(1);
            }
        }
    }

    // `--anchor`: publish a session's head hash to the transparency log and exit.
    if let Some(sid) = &args.anchor {
        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
        let session = store
            .get_session(sid)
            .map_err(|e| anyhow!(e.to_string()))?
            .ok_or_else(|| anyhow!("session {sid} not found"))?;
        let head = session
            .head_hash
            .ok_or_else(|| anyhow!("session {sid} has no entries to anchor"))?;
        let pubkey = session.pubkey_id.unwrap_or_default();
        let log = FileTransparencyLog::new(args.anchor_log.clone());
        let receipt = log
            .anchor(sid, &head, &pubkey)
            .map_err(|e| anyhow!(e.to_string()))?;
        store
            .record_anchor(
                sid,
                &receipt.log,
                receipt.index,
                &receipt.head_hash_hex,
                &receipt.entry_hash_hex,
                receipt.anchored_at_ms,
            )
            .map_err(|e| anyhow!(e.to_string()))?;
        println!(
            "anchored session {sid} → {} #{} (head {}…)",
            receipt.log,
            receipt.index,
            &receipt.head_hash_hex[..16.min(receipt.head_hash_hex.len())]
        );
        return Ok(());
    }

    // Config file (optional) provides defaults; flags win.
    let config = match &args.config {
        Some(path) => Some(Config::from_path(path)?),
        None => None,
    };

    if let Some(Command::ServeMcp(serve)) = &args.command {
        if serve.transport == ServeMcpTransport::Http {
            return Err(anyhow!(
                "MCP server HTTP transport is not implemented in this slice; use `truss serve-mcp --transport stdio`. HTTP auth and per-caller isolation are reserved for a later MCP-S3 slice."
            ));
        }
    }

    if let Some(Command::Memory(memory)) = &args.command {
        return handle_memory_command(&args, &memory.command, config.as_ref(), std::io::stdout());
    }

    if let Some(Command::Mcp(mcp)) = &args.command {
        let mut ctx = Ctx::new().with_policy(resolved_policy(&args, config.as_ref()));
        if let Some(secs) = args.timeout {
            ctx = ctx.with_timeout(std::time::Duration::from_secs(secs));
        }
        handle_mcp_command(&mcp.command, config.as_ref(), &ctx).await?;
        return Ok(());
    }

    if args.spawn_tool
        && args.contract.is_none()
        && !args.daemon
        && !args.durable
        && args.resume.is_none()
    {
        return Err(anyhow!(
            "--spawn-tool requires a store-backed run (--durable, --resume, --daemon, or --contract)"
        ));
    }

    // Effective agent settings: a flag wins; else the config's [agent] section;
    // else the built-in default. (Closes the previously-dead [agent] config.)
    let eff_system = args
        .system
        .clone()
        .or_else(|| config.as_ref().and_then(|c| c.agent.system.clone()));
    let eff_max_tokens = args
        .max_tokens
        .or_else(|| config.as_ref().and_then(|c| c.agent.max_tokens))
        .unwrap_or(4096);
    let eff_max_steps = args
        .max_steps
        .or_else(|| config.as_ref().and_then(|c| c.agent.max_steps))
        .unwrap_or(12);
    let eff_reasoning: Option<ReasoningEffort> = args
        .reasoning
        .map(Into::into)
        .or_else(|| config.as_ref().and_then(|c| c.agent.reasoning));
    let eff_temperature: Option<f32> = args
        .temperature
        .or_else(|| config.as_ref().and_then(|c| c.agent.temperature));

    let provider = match &config {
        Some(c) => c.build_provider()?,
        None => build_provider(&args)?,
    };
    let global_memory_config = resolved_memory_config(config.as_ref(), None, &args);
    let global_approval_gates = resolved_approval_gate_policy(config.as_ref(), None);
    let memory_reviewer = match &args.reviewer_model {
        Some(rm) => build_named_provider(&args, rm)?,
        None => provider.clone(),
    };
    let memory_panel_size = args.reviewers.unwrap_or(1).max(1);
    let memory_reviewers: Vec<Arc<dyn Provider>> = (0..memory_panel_size)
        .map(|_| memory_reviewer.clone())
        .collect();

    // Policy: explicit workdir flag, else config, else safe default.
    let policy = resolved_policy(&args, config.as_ref());
    let global_sandbox_config = resolved_sandbox_config(config.as_ref(), None);
    let global_sandbox =
        sandbox_from_config(&global_sandbox_config, default_checkout(&args, &policy));

    // Budget: flags override config.
    let mut limits = config
        .as_ref()
        .map(|c| c.budget_limits())
        .unwrap_or_default();
    if args.budget_usd.is_some() {
        limits.max_cost_usd = args.budget_usd;
    }
    if args.budget_tokens.is_some() {
        limits.max_total_tokens = args.budget_tokens;
    }
    let budget = BudgetMeter::new(limits);

    // Event channel: drives stdout (verbose) + optional JSONL audit log.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();

    let mut ctx = Ctx::new()
        .with_policy(policy)
        .with_sandbox(global_sandbox)
        .with_budget(budget)
        .with_events(tx.clone());
    if let Some(secs) = args.timeout {
        ctx = ctx.with_timeout(std::time::Duration::from_secs(secs));
    }

    // Tools. Global MCP servers are mounted for plain runs without a journal.
    // Durable/contract paths re-mount them with a store-backed journal so the
    // same tool names gain signed provenance.
    let mut base_tools = if args.tools {
        ToolRegistry::with_defaults()
    } else {
        ToolRegistry::new()
    };
    register_native_tools_without_journal(&mut base_tools, &args, config.as_ref());
    register_web_tools_without_journal(&mut base_tools, &args, config.as_ref());
    register_memory_tools_without_journal(
        &mut base_tools,
        &args,
        config.as_ref(),
        &global_memory_config,
    )?;
    let global_mcp_servers = mcp_servers_for(config.as_ref(), None);
    if !global_mcp_servers.is_empty() {
        base_tools
            .register_mcp_servers(&ctx, &global_mcp_servers, None)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
    }
    let tools = Arc::new(base_tools);

    if let Some(Command::ServeMcp(serve)) = &args.command {
        return handle_serve_mcp_command(ServeMcpRuntime {
            serve,
            provider,
            memory_reviewer,
            memory_reviewers,
            tools,
            args: ServeMcpSettings::from_args(
                &args,
                eff_max_tokens,
                eff_max_steps,
                eff_reasoning,
                eff_temperature,
                memory_tools_enabled(&args, config.as_ref()),
                git_tools_enabled(&args, config.as_ref()),
                github_tools_enabled(&args, config.as_ref()),
                web_tools_enabled(&args, config.as_ref()),
                web_search_config(config.as_ref()),
            ),
            config: config.map(Arc::new),
        })
        .await;
    }

    // Ctrl-C → cancel the run (aborts in-flight model call + running tools).
    let cancel = ctx.cancel_token();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[interrupt] cancelling run…");
            cancel.cancel();
        }
    });

    if let Some(Command::Eval(eval)) = &args.command {
        let contract = Arc::new(Contract::from_path(&eval.contract)?);
        let contract_memory_config =
            resolved_memory_config(config.as_ref(), Some(&contract.memory), &args);
        let contract_approval_gates =
            resolved_approval_gate_policy(config.as_ref(), Some(&contract.approval_gates));
        let mut contract_policy = contract.policy();
        let contract_sandbox_config =
            resolved_sandbox_config(config.as_ref(), Some(&contract.sandbox));
        if contract_sandbox_config.mode == SandboxMode::WorkingCopy {
            allow_self_validation_tools(&mut contract_policy);
        }
        let contract_sandbox = sandbox_from_config(
            &contract_sandbox_config,
            default_checkout(&args, &contract_policy),
        );
        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
        let memory_store = memory_store_from_config(&store, &contract_memory_config)?;

        let reviewer = match &args.reviewer_model {
            Some(rm) => build_named_provider(&args, rm)?,
            None => provider.clone(),
        };
        let panel_size = args.reviewers.or(contract.reviewers).unwrap_or(1).max(1);
        let reviewers: Vec<Arc<dyn Provider>> = (0..panel_size).map(|_| reviewer.clone()).collect();
        if panel_size > 1 {
            eprintln!("[eval] reviewer panel: {panel_size} (strict majority to pass)");
        }

        let exec_cfg = DurableConfig {
            system: contract.constraints.system.clone(),
            model: args.model.clone(),
            max_tokens: eff_max_tokens,
            max_steps: contract
                .budget
                .max_steps
                .map(|n| n as usize)
                .unwrap_or(eff_max_steps),
            retry: RetryPolicy::default(),
            reasoning: eff_reasoning,
            temperature: eff_temperature,
            max_context_msgs: args
                .max_context_msgs
                .max(contract_memory_config.projection.keep_tail),
            memory_tool: args.memory_tool,
            memory: contract_memory_config,
            persona: None,
            principal: None,
            goal_ledger: None,
            memory_cursor: truss_core::memory::MemoryCursor(0),
            memory_store: Some(memory_store.clone()),
            approval_gates: contract_approval_gates,
        };
        let mut eval_ctx = Ctx::new()
            .with_policy(contract_policy)
            .with_sandbox(contract_sandbox)
            .with_budget(ctx.budget.clone())
            .with_events(tx.clone());
        if let Some(secs) = args.timeout {
            eval_ctx = eval_ctx.with_timeout(std::time::Duration::from_secs(secs));
        }

        let report = truss_core::eval::run_paired_eval(
            &eval_ctx,
            &store,
            contract.clone(),
            truss_core::eval::PairedEvalOptions {
                repeats: eval.repeats,
                budget_slice: contract.budget_limits(),
                base_cfg: exec_cfg,
                tools: tools.clone(),
                provider: provider.clone(),
                reviewers,
                memory_store,
            },
        )
        .await
        .map_err(|e| anyhow!(e.to_string()))?;
        eprintln!("{}", report.summary);
        eprintln!(
            "[eval] parent session {} · verify with: truss --db {} --verify {}",
            report.eval_run_id, args.db, report.eval_run_id
        );
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    #[cfg(feature = "swebench-live")]
    if let Some(Command::Swebench(swebench)) = &args.command {
        use truss_bench::{
            load_swebench_tasks, BenchmarkRunner, SweBenchAdapter, SweBenchCheckout,
            SweBenchLiveConfig,
        };

        let mut tasks =
            load_swebench_tasks(&swebench.dataset).map_err(|e| anyhow!(e.to_string()))?;
        if let Some(limit) = swebench.limit {
            tasks.truncate(limit);
        }
        if tasks.is_empty() {
            return Err(anyhow!("SWE-bench dataset selection is empty"));
        }

        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
        let memory_store = memory_store_from_config(&store, &global_memory_config)?;
        let checkout_root = std::path::PathBuf::from(&swebench.checkout_root);
        let predictions_path = match swebench.solve {
            SweBenchSolveMode::Predictions => swebench
                .predictions
                .as_ref()
                .map(std::path::PathBuf::from)
                .ok_or_else(|| anyhow!("--solve predictions requires --predictions"))?,
            SweBenchSolveMode::InLoop => {
                let path = swebench
                    .predictions
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        default_in_loop_predictions_path(&checkout_root, &swebench.run_id)
                    });
                eprintln!(
                    "[swebench] generating patches in-loop with provider {} into {}",
                    args.provider,
                    path.display()
                );
                generate_swebench_predictions_in_loop(
                    &tasks,
                    &checkout_root,
                    &path,
                    provider.clone(),
                    &ctx,
                    &store,
                    InLoopSolveConfig {
                        run_id: swebench.run_id.clone(),
                        system: eff_system.clone(),
                        model: args.model.clone(),
                        max_tokens: eff_max_tokens,
                        max_steps: eff_max_steps,
                        reasoning: eff_reasoning,
                        temperature: eff_temperature,
                        max_context_msgs: args
                            .max_context_msgs
                            .max(global_memory_config.projection.keep_tail),
                        memory_tool: args.memory_tool,
                        memory: global_memory_config.clone(),
                        memory_store: memory_store.clone(),
                        approval_gates: global_approval_gates.clone(),
                    },
                )
                .await?;
                path
            }
        };
        let adapter = SweBenchAdapter::official_docker(
            SweBenchCheckout::ByInstanceRoot(checkout_root),
            SweBenchLiveConfig {
                dataset_path: std::path::PathBuf::from(&swebench.dataset),
                predictions_path: predictions_path.clone(),
                run_id: swebench.run_id.clone(),
                python: swebench.python.clone(),
                max_workers: swebench.max_workers,
            },
        );
        let grading_provider: Arc<dyn Provider> = match swebench.solve {
            SweBenchSolveMode::Predictions => provider.clone(),
            SweBenchSolveMode::InLoop => Arc::new(NoopGradingProvider),
        };
        let mut runner = BenchmarkRunner::new("swebench-verified", grading_provider, memory_store);
        runner.repeats = swebench.repeats;
        runner.base_cfg = DurableConfig {
            system: eff_system.clone(),
            model: args.model.clone(),
            max_tokens: eff_max_tokens,
            max_steps: eff_max_steps,
            retry: RetryPolicy::default(),
            reasoning: eff_reasoning,
            temperature: eff_temperature,
            max_context_msgs: args
                .max_context_msgs
                .max(global_memory_config.projection.keep_tail),
            memory_tool: args.memory_tool,
            memory: global_memory_config.clone(),
            persona: None,
            principal: None,
            goal_ledger: None,
            memory_cursor: truss_core::memory::MemoryCursor(0),
            memory_store: None,
            approval_gates: global_approval_gates.clone(),
        };

        eprintln!(
            "[swebench] running {} task(s) from local dataset {} with live SWE-bench harness",
            tasks.len(),
            swebench.dataset
        );
        let report = runner
            .run(&ctx, &store, &adapter, &tasks)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        eprintln!(
            "[swebench] resolved {}/{} · parent session {} · verify with: truss --db {} --verify {}",
            report.pass_count, report.task_count, ctx.run_id, args.db, ctx.run_id
        );
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Verified delegation: execute a contract, then independently verify it.
    if let Some(path) = &args.contract {
        use truss_core::budget::BudgetMeter;
        use truss_core::delegation::ClauseVerdict;

        let contract = Contract::from_path(path)?;
        let contract_memory_config =
            resolved_memory_config(config.as_ref(), Some(&contract.memory), &args);
        let contract_approval_gates =
            resolved_approval_gate_policy(config.as_ref(), Some(&contract.approval_gates));
        let mut contract_policy = contract.policy();
        let contract_sandbox_config =
            resolved_sandbox_config(config.as_ref(), Some(&contract.sandbox));
        if contract_sandbox_config.mode == SandboxMode::WorkingCopy {
            allow_self_validation_tools(&mut contract_policy);
        }
        let contract_sandbox = sandbox_from_config(
            &contract_sandbox_config,
            default_checkout(&args, &contract_policy),
        );
        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;

        // A fresh context carrying the contract's enforced policy + budget.
        let contract_ctx = Ctx::new()
            .with_policy(contract_policy)
            .with_sandbox(contract_sandbox)
            .with_budget(BudgetMeter::new(contract.budget_limits()))
            .with_events(tx.clone());
        let ctoken = contract_ctx.cancel_token();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\n[interrupt] cancelling contract…");
                ctoken.cancel();
            }
        });

        let reviewer = match &args.reviewer_model {
            Some(rm) => build_named_provider(&args, rm)?,
            None => provider.clone(),
        };
        // Build the reviewer panel: N independent reviewers (CLI > contract > 1).
        let panel_size = args.reviewers.or(contract.reviewers).unwrap_or(1).max(1);
        let reviewers: Vec<Arc<dyn Provider>> = (0..panel_size).map(|_| reviewer.clone()).collect();
        if panel_size > 1 {
            eprintln!("[contract] reviewer panel: {panel_size} (strict majority to pass)");
        }
        let contract_memory_store = memory_store_from_config(&store, &contract_memory_config)?;

        let exec_cfg = DurableConfig {
            system: contract.constraints.system.clone(),
            model: args.model.clone(),
            max_tokens: eff_max_tokens,
            max_steps: contract
                .budget
                .max_steps
                .map(|n| n as usize)
                .unwrap_or(eff_max_steps),
            retry: RetryPolicy::default(),
            reasoning: eff_reasoning,
            temperature: eff_temperature,
            max_context_msgs: args
                .max_context_msgs
                .max(contract_memory_config.projection.keep_tail),
            memory_tool: args.memory_tool,
            memory: contract_memory_config.clone(),
            persona: None,
            principal: None,
            goal_ledger: Some(truss_core::memory::GoalLedger::from_contract(
                contract.contract.intent.clone(),
                contract.done.iter().map(|d| d.desc.clone()).collect(),
                contract_memory_config.goal_repin_every,
            )),
            memory_cursor: truss_core::memory::MemoryCursor(0),
            memory_store: Some(contract_memory_store.clone()),
            approval_gates: contract_approval_gates,
        };

        // Select the executor: in-loop (Truss drives the provider) or a delegated
        // vendor CLI on its own subscription (ADR 0003).
        let executor = match contract.cli_executor()? {
            Some(cli) => {
                eprintln!(
                    "[contract] delegated executor: {} (subscription; vendor owns the inner loop)",
                    cli.command
                );
                Executor::DelegatedCli(cli)
            }
            None => Executor::InLoop(provider.clone()),
        };

        eprintln!("[contract] {} — executing…", contract.contract.id);
        let contract_tools = tools_for_store(
            &tools,
            &store,
            &contract_ctx,
            StoreToolOptions {
                memory_tool: args.memory_tool,
                memory_tools: memory_tools_enabled(&args, config.as_ref()),
                memory_config: contract_memory_config.clone(),
                memory_reviewers: reviewers.clone(),
                spawn_tool: args.spawn_tool,
                provider: provider.clone(),
                spawn_cfg: exec_cfg.clone(),
                self_check: contract_sandbox_config.mode == SandboxMode::WorkingCopy,
                session_id: contract.contract.id.clone(),
                declared_test_command: declared_test_command(&contract),
                mcp_servers: mcp_servers_for(config.as_ref(), Some(&contract.mcp)),
                git_tools: git_tools_enabled(&args, config.as_ref()),
                github_tools: github_tools_enabled(&args, config.as_ref()),
                web_tools: web_tools_enabled(&args, config.as_ref()),
                web_search_config: web_search_config(config.as_ref()),
            },
        )
        .await?;
        let close_learning = if contract_memory_config.extraction
            == truss_core::memory::LessonExtractionPolicy::Off
        {
            None
        } else {
            Some(delegation::ContractCloseLearning {
                memory_store: contract_memory_store,
                extractor: provider.clone(),
            })
        };
        let verdict = delegation::run_contract_with_close_learning(
            executor,
            reviewers,
            contract_tools,
            &contract_ctx,
            &store,
            &contract,
            &exec_cfg,
            close_learning,
        )
        .await
        .map_err(|e| anyhow!(e.to_string()))?;

        let head = store
            .get_session(&contract.contract.id)
            .ok()
            .flatten()
            .and_then(|s| s.head_hash)
            .map(hex::encode)
            .unwrap_or_default();

        println!("{}", delegation::render_bundle(&verdict, &head));
        if verdict.overall != ClauseVerdict::Pass {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Resume daemon: poll suspended sessions and resume any whose await is met.
    if args.daemon {
        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
        let memory_store = memory_store_from_config(&store, &global_memory_config)?;
        let cfg = DurableConfig {
            system: eff_system.clone(),
            model: args.model.clone(),
            max_tokens: eff_max_tokens,
            max_steps: eff_max_steps,
            retry: RetryPolicy::default(),
            reasoning: eff_reasoning,
            temperature: eff_temperature,
            max_context_msgs: args
                .max_context_msgs
                .max(global_memory_config.projection.keep_tail),
            memory_tool: args.memory_tool,
            memory: global_memory_config.clone(),
            persona: None,
            principal: None,
            goal_ledger: None,
            memory_cursor: truss_core::memory::MemoryCursor(0),
            memory_store: Some(memory_store),
            approval_gates: global_approval_gates.clone(),
        };
        eprintln!(
            "[daemon] polling every {}s (Ctrl-C to stop)",
            args.poll_secs
        );
        loop {
            if ctx.is_cancelled() {
                eprintln!("[daemon] stopped");
                return Ok(());
            }
            let suspended = store
                .list_suspended_sessions()
                .map_err(|e| anyhow!(e.to_string()))?;
            for sid in suspended {
                let run_tools = tools_for_store(
                    &tools,
                    &store,
                    &ctx,
                    StoreToolOptions {
                        memory_tool: args.memory_tool,
                        memory_tools: memory_tools_enabled(&args, config.as_ref()),
                        memory_config: global_memory_config.clone(),
                        memory_reviewers: memory_reviewers.clone(),
                        spawn_tool: args.spawn_tool,
                        provider: provider.clone(),
                        spawn_cfg: cfg.clone(),
                        self_check: global_sandbox_config.mode == SandboxMode::WorkingCopy,
                        session_id: sid.clone(),
                        declared_test_command: None,
                        mcp_servers: global_mcp_servers.clone(),
                        git_tools: git_tools_enabled(&args, config.as_ref()),
                        github_tools: github_tools_enabled(&args, config.as_ref()),
                        web_tools: web_tools_enabled(&args, config.as_ref()),
                        web_search_config: web_search_config(config.as_ref()),
                    },
                )
                .await?;
                match durable::run(provider.clone(), run_tools, &ctx, &store, &sid, &cfg, None)
                    .await
                {
                    Ok(DurableOutcome::Completed { .. }) => eprintln!("[daemon] {sid} completed"),
                    Ok(DurableOutcome::Suspended { await_desc, .. }) => {
                        eprintln!("[daemon] {sid} re-suspended · awaiting {await_desc}")
                    }
                    Ok(DurableOutcome::StillWaiting { .. }) => {}
                    Err(e) => eprintln!("[daemon] {sid} error: {e}"),
                }
            }
            let token = ctx.cancel_token();
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(args.poll_secs)) => {}
                _ = token.cancelled() => {}
            }
        }
    }

    // Durable path: SQLite-journaled run with suspend/resume.
    if args.durable || args.resume.is_some() {
        let signer =
            Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
        let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
        let memory_store = memory_store_from_config(&store, &global_memory_config)?;
        let session_id = args
            .resume
            .clone()
            .or_else(|| args.session.clone())
            .unwrap_or_else(|| ctx.run_id.clone());
        let prompt = if args.resume.is_some() {
            None
        } else {
            let p = args.prompt.join(" ");
            if p.is_empty() {
                return Err(anyhow!("a prompt is required to start a durable run"));
            }
            Some(p)
        };
        let cfg = DurableConfig {
            system: eff_system.clone(),
            model: args.model.clone(),
            max_tokens: eff_max_tokens,
            max_steps: eff_max_steps,
            retry: RetryPolicy::default(),
            reasoning: eff_reasoning,
            temperature: eff_temperature,
            max_context_msgs: args
                .max_context_msgs
                .max(global_memory_config.projection.keep_tail),
            memory_tool: args.memory_tool,
            memory: global_memory_config.clone(),
            persona: None,
            principal: None,
            goal_ledger: None,
            memory_cursor: truss_core::memory::MemoryCursor(0),
            memory_store: Some(memory_store),
            approval_gates: global_approval_gates.clone(),
        };
        eprintln!(
            "[durable] session {session_id} · signing key {}…",
            &store.pubkey()[..16.min(store.pubkey().len())]
        );
        let run_tools = tools_for_store(
            &tools,
            &store,
            &ctx,
            StoreToolOptions {
                memory_tool: args.memory_tool,
                memory_tools: memory_tools_enabled(&args, config.as_ref()),
                memory_config: global_memory_config,
                memory_reviewers,
                spawn_tool: args.spawn_tool,
                provider: provider.clone(),
                spawn_cfg: cfg.clone(),
                self_check: global_sandbox_config.mode == SandboxMode::WorkingCopy,
                session_id: session_id.clone(),
                declared_test_command: None,
                mcp_servers: global_mcp_servers.clone(),
                git_tools: git_tools_enabled(&args, config.as_ref()),
                github_tools: github_tools_enabled(&args, config.as_ref()),
                web_tools: web_tools_enabled(&args, config.as_ref()),
                web_search_config: web_search_config(config.as_ref()),
            },
        )
        .await?;
        let outcome = durable::run(provider, run_tools, &ctx, &store, &session_id, &cfg, prompt)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        match outcome {
            DurableOutcome::Completed { final_text } => {
                println!("{final_text}");
                eprintln!("[done] session {session_id} completed");
            }
            DurableOutcome::Suspended { await_desc, reason } => {
                eprintln!("[suspended] {reason} · awaiting {await_desc}");
                eprintln!("resume with: truss --resume {session_id} --db {}", args.db);
            }
            DurableOutcome::StillWaiting { await_desc } => {
                eprintln!("[waiting] awaiting {await_desc}; resume again later or run --daemon");
            }
        }
        return Ok(());
    }

    let agent = Agent {
        provider,
        tools,
        system: eff_system.clone(),
        model: args.model.clone(),
        max_tokens: eff_max_tokens,
        max_steps: eff_max_steps,
        temperature: eff_temperature,
        reasoning: eff_reasoning,
        retry: RetryPolicy::default(),
        escalate_at: 0.8,
    };

    // Run on a task so we can drain events live; the task owns `ctx`, so when it
    // finishes the channel closes and the drain loop ends.
    let prompt = args.prompt.join(" ");
    if prompt.is_empty() {
        return Err(anyhow!("a prompt is required"));
    }
    let verbose = args.verbose;
    let handle = tokio::spawn(async move { agent.run(&ctx, prompt).await });

    let mut log = match &args.log {
        Some(path) => Some(RunLog::open(path).await?),
        None => None,
    };

    while let Some(event) = rx.recv().await {
        if let Some(l) = log.as_mut() {
            l.record(&event).await;
        }
        if verbose {
            print_event(&event);
        } else {
            // Default: stream the assistant's text to stdout as it arrives.
            match &event {
                AgentEvent::AssistantTextDelta { text, .. } => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::AssistantText { text, .. } if !text.is_empty() => println!(),
                _ => {}
            }
        }
    }

    match handle.await.expect("run task panicked") {
        Ok(outcome) => {
            // The answer already streamed to stdout above (delta events); only the
            // run summary remains.
            eprintln!(
                "[done] {} steps · {} tokens · ${:.4}",
                outcome.usage.steps,
                outcome.usage.total_tokens(),
                outcome.usage.cost_usd
            );
            Ok(())
        }
        Err(HarnessError::Cancelled) => {
            eprintln!("[cancelled]");
            std::process::exit(130);
        }
        Err(e) => Err(anyhow!(e.to_string())),
    }
}

#[cfg(feature = "swebench-live")]
#[derive(Clone)]
struct NoopGradingProvider;

#[cfg(feature = "swebench-live")]
#[async_trait]
impl Provider for NoopGradingProvider {
    fn name(&self) -> &str {
        "swebench-official-grader"
    }

    fn default_model(&self) -> &str {
        "swebench-official-grader"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> std::result::Result<CompletionResponse, truss_core::error::ProviderError> {
        Ok(CompletionResponse {
            content: vec![truss_core::message::Block::Text(
                "official SWE-bench grading is the pass/fail authority".into(),
            )],
            stop_reason: StopReason::EndTurn,
            model: self.default_model().into(),
            usage: Usage::default(),
        })
    }
}

#[cfg(feature = "swebench-live")]
struct InLoopSolveConfig {
    run_id: String,
    system: Option<String>,
    model: Option<String>,
    max_tokens: u32,
    max_steps: usize,
    reasoning: Option<ReasoningEffort>,
    temperature: Option<f32>,
    max_context_msgs: usize,
    memory_tool: bool,
    memory: MemoryConfig,
    memory_store: Arc<dyn truss_core::memory::MemoryStore>,
    approval_gates: ApprovalGatePolicy,
}

#[cfg(feature = "swebench-live")]
async fn generate_swebench_predictions_in_loop(
    tasks: &[truss_bench::SweBenchTask],
    checkout_root: &Path,
    predictions_path: &Path,
    provider: Arc<dyn Provider>,
    ctx: &Ctx,
    store: &Store,
    config: InLoopSolveConfig,
) -> Result<()> {
    if let Some(parent) = predictions_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut rows = Vec::with_capacity(tasks.len());
    for task in tasks {
        let workdir = checkout_root.join(safe_swebench_id(&task.instance_id));
        prepare_swebench_checkout(task, &workdir)?;
        let diff = match solve_swebench_task_in_loop(
            task,
            &workdir,
            provider.clone(),
            ctx,
            store,
            &config,
        )
        .await
        {
            Ok(diff) => diff,
            Err(err) => {
                eprintln!("[swebench] {} in-loop solve error: {err}", task.instance_id);
                git_output(&workdir, &["diff", "--binary", "HEAD"]).unwrap_or_default()
            }
        };
        let patch_bytes = diff.len();
        eprintln!(
            "[swebench] {} generated {} patch bytes",
            task.instance_id, patch_bytes
        );
        rows.push(serde_json::json!({
            "instance_id": task.instance_id,
            "model_name_or_path": format!("truss-in-loop-{}", provider.default_model()),
            "model_patch": diff,
        }));
    }
    let mut out = String::new();
    for row in rows {
        out.push_str(&serde_json::to_string(&row)?);
        out.push('\n');
    }
    std::fs::write(predictions_path, out)?;
    Ok(())
}

#[cfg(feature = "swebench-live")]
async fn solve_swebench_task_in_loop(
    task: &truss_bench::SweBenchTask,
    workdir: &Path,
    provider: Arc<dyn Provider>,
    parent_ctx: &Ctx,
    store: &Store,
    config: &InLoopSolveConfig,
) -> Result<String> {
    let contract = in_loop_swebench_contract(task, workdir, &config.run_id);
    let mut exec_cfg = DurableConfig {
        system: config
            .system
            .clone()
            .or_else(|| Some(in_loop_swebench_system())),
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        max_steps: config.max_steps,
        retry: RetryPolicy::default(),
        reasoning: config.reasoning,
        temperature: config.temperature,
        max_context_msgs: config.max_context_msgs,
        memory_tool: config.memory_tool,
        memory: config.memory.clone(),
        persona: None,
        principal: None,
        goal_ledger: Some(truss_core::memory::GoalLedger::from_contract(
            task.problem_statement.clone(),
            vec!["Produce a non-empty git diff that addresses the task.".into()],
            config.memory.goal_repin_every,
        )),
        memory_cursor: truss_core::memory::MemoryCursor(0),
        memory_store: Some(config.memory_store.clone()),
        approval_gates: config.approval_gates.clone(),
    };
    if exec_cfg.max_steps == 0 {
        exec_cfg.max_steps = 1;
    }
    let gen_ctx = parent_ctx.child().with_policy(contract.policy());
    let tools = Arc::new(ToolRegistry::with_defaults());
    let verdict = delegation::run_contract_with_close_learning(
        Executor::InLoop(provider),
        Vec::new(),
        tools,
        &gen_ctx,
        store,
        &contract,
        &exec_cfg,
        None,
    )
    .await
    .map_err(|e| anyhow!(e.to_string()))?;
    eprintln!(
        "[swebench] {} in-loop verdict: {:?}",
        task.instance_id, verdict.overall
    );
    git_output(workdir, &["diff", "--binary", "HEAD"])
}

#[cfg(feature = "swebench-live")]
fn in_loop_swebench_contract(
    task: &truss_bench::SweBenchTask,
    workdir: &Path,
    run_id: &str,
) -> Contract {
    let workdir = absolute_path(workdir);
    Contract {
        contract: truss_core::contract::Meta {
            id: format!(
                "swebench-inloop-{}-{}",
                safe_swebench_id(run_id),
                safe_swebench_id(&task.instance_id)
            ),
            title: format!("SWE-bench in-loop solve {}", task.instance_id),
            intent: format!(
                "{}\n\nWork in repository: {}\nBase commit: {}\nDo not commit changes. Leave the candidate solution as an uncommitted working-tree diff.",
                task.problem_statement,
                workdir.display(),
                task.base_commit
            ),
        },
        done: vec![truss_core::contract::DoneClause {
            kind: "check".into(),
            desc: "A non-empty candidate patch exists in git diff.".into(),
            run: Some(format!(
                "cd {} && git diff --quiet HEAD && exit 1 || exit 0",
                shell_quote_path_cli(&workdir)
            )),
            expect_exit: Some(0),
            expect_contains: Vec::new(),
            reviewer_context: Vec::new(),
        }],
        budget: truss_core::contract::BudgetSpec {
            max_steps: Some(24),
            ..Default::default()
        },
        constraints: truss_core::contract::ConstraintSpec {
            write_scope: Some(vec![workdir.clone()]),
            read_scope: Some(vec![workdir]),
            network: Some("none".into()),
            allow_tools: Some(vec![
                "read_file".into(),
                "write_file".into(),
                "list_dir".into(),
                "bash".into(),
            ]),
            forbidden: None,
            system: Some(in_loop_swebench_system()),
        },
        reviewers: Some(0),
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    }
}

#[cfg(feature = "swebench-live")]
fn in_loop_swebench_system() -> String {
    "You are Truss's in-loop SWE-bench solver. Use the available filesystem and bash tools to inspect the repository, edit files, and run focused tests. Keep all changes inside the provided repository. Do not commit. Finish only after leaving the candidate fix as an uncommitted git diff.".into()
}

#[cfg(feature = "swebench-live")]
fn prepare_swebench_checkout(task: &truss_bench::SweBenchTask, workdir: &Path) -> Result<()> {
    if !workdir.join(".git").is_dir() {
        if let Some(parent) = workdir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let repo_url = format!("https://github.com/{}.git", task.repo);
        run_git(
            Path::new("."),
            &[
                "clone",
                "--no-checkout",
                repo_url.as_str(),
                &workdir.to_string_lossy(),
            ],
        )?;
    }
    run_git(workdir, &["reset", "--hard"])?;
    run_git(workdir, &["clean", "-fdx"])?;
    if run_git(workdir, &["checkout", "--force", &task.base_commit]).is_err() {
        run_git(
            workdir,
            &["fetch", "--depth", "1", "origin", &task.base_commit],
        )?;
        run_git(workdir, &["checkout", "--force", &task.base_commit])?;
    }
    Ok(())
}

#[cfg(feature = "swebench-live")]
fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "git {} failed in {}: {}{}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(feature = "swebench-live")]
fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "git {} failed in {}: {}{}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(feature = "swebench-live")]
fn default_in_loop_predictions_path(checkout_root: &Path, run_id: &str) -> PathBuf {
    checkout_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            "truss-inloop-{}-predictions.jsonl",
            safe_swebench_id(run_id)
        ))
}

#[cfg(feature = "swebench-live")]
fn safe_swebench_id(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(feature = "swebench-live")]
fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(feature = "swebench-live")]
fn shell_quote_path_cli(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

#[cfg(all(test, feature = "swebench-live"))]
mod swebench_in_loop_tests {
    use super::*;

    fn task() -> truss_bench::SweBenchTask {
        truss_bench::SweBenchTask {
            instance_id: "psf__requests-1142".into(),
            repo: "psf/requests".into(),
            base_commit: "0123456789012345678901234567890123456789".into(),
            problem_statement: "Fix the regression.".into(),
            patch: String::new(),
            test_patch: String::new(),
            fail_to_pass: vec!["tests/test_requests.py::test_fix".into()],
            pass_to_pass: Vec::new(),
            hints_text: None,
            created_at: None,
            version: None,
            environment_setup_commit: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn in_loop_predictions_default_next_to_checkout_root() {
        let path =
            default_in_loop_predictions_path(Path::new("/tmp/truss/checkouts"), "inloop run/1");

        assert_eq!(
            path,
            PathBuf::from("/tmp/truss/truss-inloop-inloop_run_1-predictions.jsonl")
        );
    }

    #[test]
    fn in_loop_contract_scopes_tools_to_task_checkout() {
        let workdir = Path::new("/tmp/truss/checkouts/psf__requests-1142");
        let contract = in_loop_swebench_contract(&task(), workdir, "run/1");

        assert_eq!(
            contract.contract.id,
            "swebench-inloop-run_1-psf__requests-1142"
        );
        assert!(contract.contract.intent.contains("Do not commit changes"));
        assert_eq!(
            contract.constraints.write_scope,
            Some(vec![workdir.to_path_buf()])
        );
        assert_eq!(
            contract.constraints.read_scope,
            Some(vec![workdir.to_path_buf()])
        );
        assert_eq!(
            contract.constraints.allow_tools,
            Some(vec![
                "read_file".into(),
                "write_file".into(),
                "list_dir".into(),
                "bash".into(),
            ])
        );
        assert!(contract.done[0]
            .run
            .as_deref()
            .unwrap()
            .contains("git diff --quiet HEAD"));
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod compaction_cli_tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;
    use truss_core::budget::BudgetSnapshot;
    use truss_core::delegation::{apply_memory_verdict, ClauseResult, ClauseVerdict, Verdict};
    use truss_core::memory::{
        CompactionPolicyLayer, EffectiveBudgetPolicyLayer, EntryKind, LedgerRange, LedgerRef,
        MemoryDraft, MemoryStore, SqliteMemoryStore, VerificationPolicy,
    };
    use truss_core::message::Block;
    use truss_core::provider::{
        CompletionRequest, CompletionResponse, ModelCapabilities, StopReason, ToolDef, Usage,
    };
    use truss_core::tools::{Tool, ToolRegistry};

    #[derive(Clone)]
    struct MockProvider {
        calls: Arc<Mutex<usize>>,
        script: Vec<CompletionResponse>,
        context_window: u32,
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
            ModelCapabilities {
                context_window: self.context_window,
                ..ModelCapabilities::default()
            }
        }

        async fn complete(
            &self,
            _ctx: &Ctx,
            _req: &CompletionRequest,
        ) -> std::result::Result<CompletionResponse, truss_core::error::ProviderError> {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            self.script.get(index).cloned().ok_or_else(|| {
                truss_core::error::ProviderError::Config(format!(
                    "script exhausted at call {index}"
                ))
            })
        }
    }

    #[derive(Clone)]
    struct PlaybookEvalProvider {
        use_playbook_when_recalled: bool,
    }

    #[async_trait]
    impl Provider for PlaybookEvalProvider {
        fn name(&self) -> &str {
            "playbook-eval"
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        async fn complete(
            &self,
            _ctx: &Ctx,
            req: &CompletionRequest,
        ) -> std::result::Result<CompletionResponse, truss_core::error::ProviderError> {
            let prompt = completion_request_text(req);
            let system = req.system.clone().unwrap_or_default();
            if system.contains("Extract only durable") {
                return Ok(text(r#"{"lessons":[]}"#, 1));
            }
            if prompt.contains("used the candidate playbook")
                || (self.use_playbook_when_recalled && prompt.contains("[L2 verified playbooks]"))
            {
                Ok(text("used the candidate playbook", 1))
            } else {
                Ok(text("missed the candidate playbook", 1))
            }
        }
    }

    struct PlaybookEvalReviewer;

    #[async_trait]
    impl Provider for PlaybookEvalReviewer {
        fn name(&self) -> &str {
            "playbook-eval-reviewer"
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        async fn complete(
            &self,
            _ctx: &Ctx,
            req: &CompletionRequest,
        ) -> std::result::Result<CompletionResponse, truss_core::error::ProviderError> {
            let prompt = completion_request_text(req);
            let met = prompt.contains("used the candidate playbook");
            Ok(text(
                &format!(r#"{{"met": {met}, "reasoning": "scripted eval review"}}"#),
                1,
            ))
        }
    }

    fn completion_request_text(req: &CompletionRequest) -> String {
        let mut text = req
            .system_blocks
            .iter()
            .map(|block| block.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        for message in &req.messages {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&message.text());
        }
        text
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn def(&self) -> ToolDef {
            ToolDef {
                name: "echo".into(),
                description: "returns the call id payload".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"]
                }),
            }
        }

        async fn call(
            &self,
            _ctx: &Ctx,
            input: serde_json::Value,
        ) -> std::result::Result<String, truss_core::error::ToolError> {
            Ok(input["value"].as_str().unwrap_or("").to_string())
        }
    }

    fn tool_use(id: &str, value: &str, input_tokens: u64) -> CompletionResponse {
        CompletionResponse {
            content: vec![Block::ToolUse {
                id: id.into(),
                name: "echo".into(),
                input: json!({ "value": value }),
            }],
            stop_reason: StopReason::ToolUse,
            model: "mock-model".into(),
            usage: Usage {
                input_tokens,
                output_tokens: 1,
            },
        }
    }

    fn text(text: &str, input_tokens: u64) -> CompletionResponse {
        CompletionResponse {
            content: vec![Block::Text(text.into())],
            stop_reason: StopReason::EndTurn,
            model: "mock-model".into(),
            usage: Usage {
                input_tokens,
                output_tokens: 1,
            },
        }
    }

    fn review_response(met: Option<bool>) -> CompletionResponse {
        match met {
            Some(met) => text(
                &format!(r#"{{"met": {met}, "reasoning": "scripted review"}}"#),
                1,
            ),
            None => text("not-json", 1),
        }
    }

    fn args_from(flags: &[&str]) -> Args {
        let mut argv = vec!["truss"];
        argv.extend_from_slice(flags);
        argv.push("prompt");
        Args::parse_from(argv)
    }

    fn ledger_args(flags: &[&str]) -> Args {
        let mut argv = vec!["truss"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
    }

    fn review_args(flags: &[&str]) -> Args {
        let mut argv = vec!["truss"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
    }

    fn playbook_args(flags: &[&str]) -> Args {
        let mut argv = vec!["truss"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
    }

    fn run_playbook(args: &Args) -> String {
        let Some(Command::Playbook(playbook)) = &args.command else {
            panic!("expected playbook command");
        };
        let mut output = Vec::new();
        handle_playbook_command(args, &playbook.command, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    async fn run_playbook_promote_with_reviews(
        args: &Args,
        reviews: Vec<CompletionResponse>,
    ) -> (String, bool) {
        let Some(Command::Playbook(PlaybookArgs {
            command: PlaybookCommand::Promote {
                id_or_key, json, ..
            },
        })) = &args.command
        else {
            panic!("expected playbook promote command");
        };
        let provider = Arc::new(MockProvider {
            calls: Arc::new(Mutex::new(0)),
            script: reviews,
            context_window: 128_000,
        });
        let reviewers: Vec<Arc<dyn Provider>> = (0..args.reviewers.unwrap_or(1).max(1))
            .map(|_| provider.clone() as Arc<dyn Provider>)
            .collect();
        let mut output = Vec::new();
        let promoted = handle_playbook_promote_command(
            args,
            id_or_key,
            *json,
            reviewers,
            &Ctx::new(),
            &mut output,
        )
        .await
        .unwrap();
        (String::from_utf8(output).unwrap(), promoted)
    }

    async fn run_playbook_promote_with_eval(
        args: &Args,
        use_playbook_when_recalled: bool,
    ) -> (String, bool) {
        let Some(Command::Playbook(PlaybookArgs {
            command:
                PlaybookCommand::Promote {
                    id_or_key,
                    json,
                    repeats,
                    ..
                },
        })) = &args.command
        else {
            panic!("expected playbook promote command");
        };
        let contract = Arc::new(playbook_eval_contract());
        let provider = Arc::new(PlaybookEvalProvider {
            use_playbook_when_recalled,
        });
        let mut output = Vec::new();
        let promoted = handle_playbook_eval_promote_command(
            args,
            id_or_key,
            *json,
            PlaybookEvalPromoteOptions {
                contract: contract.clone(),
                repeats: repeats.unwrap_or(1),
                budget_slice: contract.budget_limits(),
                base_cfg: DurableConfig {
                    max_steps: 2,
                    memory: MemoryConfig::default(),
                    ..DurableConfig::default()
                },
                tools: Arc::new(ToolRegistry::new()),
                provider,
                reviewers: vec![Arc::new(PlaybookEvalReviewer)],
                ctx: Ctx::new(),
            },
            &mut output,
        )
        .await
        .unwrap();
        (String::from_utf8(output).unwrap(), promoted)
    }

    fn playbook_eval_contract() -> Contract {
        Contract {
            contract: truss_core::contract::Meta {
                id: "playbook-eval-contract".into(),
                title: "Playbook eval contract".into(),
                intent: "Use the Rust Review candidate playbook for cargo fmt validation.".into(),
            },
            done: vec![truss_core::contract::DoneClause {
                kind: "review".into(),
                desc: "The executor applied the candidate guidance.".into(),
                run: None,
                expect_exit: None,
                expect_contains: Vec::new(),
                reviewer_context: Vec::new(),
            }],
            budget: truss_core::contract::BudgetSpec {
                max_steps: Some(3),
                ..truss_core::contract::BudgetSpec::default()
            },
            constraints: truss_core::contract::ConstraintSpec::default(),
            reviewers: Some(1),
            memory: Default::default(),
            sandbox: Default::default(),
            approval_gates: Default::default(),
            mcp: Default::default(),
            executor: truss_core::contract::ExecutorSpec::default(),
        }
    }

    fn temp_text_path(label: &str, ext: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "truss-{label}-{}-{unique}.{ext}",
            std::process::id()
        ))
    }

    fn config_from(text: &str) -> Config {
        toml::from_str(text).expect("config parses")
    }

    fn fake_mcp_config() -> Config {
        let fake = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/fake-mcp-server.py");
        config_from(&format!(
            r#"
[provider]
kind = "openai"
model = "dummy"

[[mcp.servers]]
name = "demo"
transport = "stdio"
command = "python3"
args = [{}]
allow_tools = ["echo"]

[mcp.servers.env_headers]
"authorization" = "IGNORED_FOR_STDIO"
"#,
            serde_json::to_string(&fake.to_string_lossy()).unwrap()
        ))
    }

    #[test]
    fn mcp_cli_list_summarizes_config_without_secrets() {
        let config = fake_mcp_config();
        let output = mcp_list_output(Some(&config));

        assert_eq!(output.servers.len(), 1);
        let server = &output.servers[0];
        assert_eq!(server.name, "demo");
        assert_eq!(server.transport, "stdio");
        assert_eq!(server.command.as_deref(), Some("python3"));
        assert_eq!(
            server.allow_tools.as_deref(),
            Some(&["echo".to_string()][..])
        );
        assert_eq!(
            server.env_headers,
            vec![McpEnvHeaderSummary {
                header: "authorization".into(),
                env_var: "IGNORED_FOR_STDIO".into(),
            }]
        );
    }

    #[tokio::test]
    async fn mcp_cli_probe_mounts_and_reports_advertised_tools() {
        let config = fake_mcp_config();
        let output = mcp_probe_output(Some(&config), &Ctx::new(), "demo")
            .await
            .expect("probe");

        assert_eq!(output.server.name, "demo");
        assert_eq!(output.tools.len(), 1);
        assert_eq!(output.tools[0].name, "mcp:demo:echo");
        assert_eq!(output.tools[0].input_schema["required"][0], "message");
    }

    #[test]
    fn mcp_subcommand_parses_without_prompt_or_provider() {
        let args = Args::parse_from(["truss", "--config", "harness.toml", "mcp", "list", "--json"]);
        match args.command {
            Some(Command::Mcp(McpArgs {
                command: McpCommand::List { json },
            })) => assert!(json),
            other => panic!("expected mcp list, got {other:?}"),
        }
    }

    #[test]
    fn playbook_subcommand_parses_without_prompt_or_provider() {
        let args = Args::parse_from(["truss", "playbook", "list", "--json"]);
        match args.command {
            Some(Command::Playbook(PlaybookArgs {
                command: PlaybookCommand::List { json, .. },
            })) => assert!(json),
            other => panic!("expected playbook list, got {other:?}"),
        }
    }

    #[test]
    fn playbook_import_list_show_and_revoke() {
        let db = temp_db_path("playbook");
        let key = temp_text_path("playbook-key", "key");
        let source = temp_text_path("skill", "md");
        std::fs::write(
            &source,
            r#"---
description: Import docs safely.
---

# Safe Import

## Trigger
Use when importing a markdown skill.

## Prerequisites
- A source file exists.

## Procedure
1. Read the source.
2. Store an unverified playbook.

## Checks
- The playbook is listed as unverified.

## Examples
- truss playbook import skill.md

## Known Failures
- Missing source file.
"#,
        )
        .unwrap();

        let import = run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "import",
            source.to_str().unwrap(),
            "--key",
            "safe-import",
        ]));
        assert!(import.contains("imported playbook"));
        assert!(import.contains("trust: unverified"));
        assert!(import.contains("title: Safe Import"));

        let list = run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "list",
        ]));
        assert!(list.contains("safe-import"));
        assert!(list.contains("unverified"));

        let show = run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "show",
            "safe-import",
        ]));
        assert!(show.contains("title: Safe Import"));
        assert!(show.contains("procedure:"));
        assert!(show.contains("Store an unverified playbook."));

        let revoke = run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "revoke",
            "safe-import",
            "--reason",
            "regressed",
        ]));
        assert!(revoke.contains("revoked playbook"));
        assert!(revoke.contains("trust: stale"));

        let show_after = run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "show",
            "safe-import",
        ]));
        assert!(show_after.contains("trust: stale"));

        std::fs::remove_file(db).ok();
        std::fs::remove_file(key).ok();
        std::fs::remove_file(source).ok();
    }

    #[tokio::test]
    async fn playbook_promote_json_majority_pass_promotes_with_signed_event() {
        let db = temp_db_path("playbook-promote-pass");
        let key = temp_text_path("playbook-promote-key", "key");
        let source = temp_text_path("promotable-skill", "md");
        std::fs::write(
            &source,
            r#"# Rust Review

## Purpose
Review Rust changes safely.

## Procedure
1. Run cargo fmt.
2. Check the signed ledger.

## Checks
- cargo fmt passes.
"#,
        )
        .unwrap();
        run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "import",
            source.to_str().unwrap(),
            "--key",
            "rust-review",
        ]));

        let (output, promoted) = run_playbook_promote_with_reviews(
            &playbook_args(&[
                "--db",
                db.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
                "--reviewers",
                "3",
                "playbook",
                "promote",
                "rust-review",
                "--json",
            ]),
            vec![
                review_response(Some(true)),
                review_response(Some(false)),
                review_response(Some(true)),
            ],
        )
        .await;
        assert!(promoted);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["promoted"], true);
        assert_eq!(json["event_kind"], "playbook_promoted");
        assert_eq!(json["verdict_overall"], "Pass");
        assert_eq!(json["playbook"]["trust"], "verified");
        assert_eq!(json["playbook"]["key"], "rust-review");
        let promote_session = json["promote_session"].as_str().unwrap();
        let verdict_seq = json["verdict_ref"]["seq"].as_i64().unwrap();

        let store = Store::open(
            db.to_str().unwrap(),
            Arc::new(Signer::load_or_create(key.to_str().unwrap()).unwrap()),
        )
        .unwrap();
        let memory = SqliteMemoryStore::from_store(&store).unwrap();
        let promoted_entry = find_playbook(&memory, "rust-review").unwrap();
        assert_eq!(promoted_entry.trust, Trust::Verified);
        assert_eq!(
            promoted_entry.provenance.verified_by_ref,
            Some(LedgerRef {
                session: promote_session.into(),
                seq: verdict_seq,
            })
        );
        let event = store
            .entries(promote_session)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "playbook_promoted")
            .expect("playbook promoted event");
        assert!(!event.signature.is_empty());
        assert_eq!(event.payload["overall"], "Pass");
        assert!(store.verify_chain(promote_session).unwrap().is_ok());

        std::fs::remove_file(db).ok();
        std::fs::remove_file(key).ok();
        std::fs::remove_file(source).ok();
    }

    #[tokio::test]
    async fn playbook_promote_fail_or_unverifiable_leaves_unverified_with_signed_event() {
        for (case, response, expected) in [
            ("fail", review_response(Some(false)), "Fail"),
            ("unverifiable", review_response(None), "Unverifiable"),
        ] {
            let db = temp_db_path(&format!("playbook-promote-{case}"));
            let key = temp_text_path(&format!("playbook-promote-{case}-key"), "key");
            let source = temp_text_path(&format!("promotable-skill-{case}"), "md");
            std::fs::write(
                &source,
                r#"# Risky Playbook

## Purpose
Claim something reviewers may reject.

## Procedure
1. Skip validation.
"#,
            )
            .unwrap();
            run_playbook(&playbook_args(&[
                "--db",
                db.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
                "playbook",
                "import",
                source.to_str().unwrap(),
                "--key",
                "risky-playbook",
            ]));

            let (output, promoted) = run_playbook_promote_with_reviews(
                &playbook_args(&[
                    "--db",
                    db.to_str().unwrap(),
                    "--key",
                    key.to_str().unwrap(),
                    "playbook",
                    "promote",
                    "risky-playbook",
                    "--json",
                ]),
                vec![response],
            )
            .await;
            assert!(!promoted);
            let json: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(json["promoted"], false);
            assert_eq!(json["event_kind"], "playbook_promotion_rejected");
            assert_eq!(json["verdict_overall"], expected);
            assert_eq!(json["playbook"]["trust"], "unverified");
            let promote_session = json["promote_session"].as_str().unwrap();

            let store = Store::open(
                db.to_str().unwrap(),
                Arc::new(Signer::load_or_create(key.to_str().unwrap()).unwrap()),
            )
            .unwrap();
            let memory = SqliteMemoryStore::from_store(&store).unwrap();
            let draft = find_playbook(&memory, "risky-playbook").unwrap();
            assert_eq!(draft.trust, Trust::Unverified);
            assert!(draft.provenance.verified_by_ref.is_none());
            let event = store
                .entries(promote_session)
                .unwrap()
                .into_iter()
                .find(|entry| entry.kind == "playbook_promotion_rejected")
                .expect("playbook rejection event");
            assert!(!event.signature.is_empty());
            assert_eq!(event.payload["overall"], expected);
            assert!(store.verify_chain(promote_session).unwrap().is_ok());

            std::fs::remove_file(db).ok();
            std::fs::remove_file(key).ok();
            std::fs::remove_file(source).ok();
        }
    }

    #[tokio::test]
    async fn playbook_eval_promote_positive_delta_promotes_with_signed_event() {
        let db = temp_db_path("playbook-eval-promote-pass");
        let key = temp_text_path("playbook-eval-promote-key", "key");
        let source = temp_text_path("eval-promotable-skill", "md");
        std::fs::write(
            &source,
            r#"# Rust Review

## Purpose
Use the Rust Review candidate playbook for cargo fmt validation.

## Procedure
1. Run cargo fmt.
2. Report that the candidate playbook was used.

## Checks
- cargo fmt passes.
"#,
        )
        .unwrap();
        run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "import",
            source.to_str().unwrap(),
            "--key",
            "rust-review",
        ]));

        let (output, promoted) = run_playbook_promote_with_eval(
            &playbook_args(&[
                "--db",
                db.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
                "playbook",
                "promote",
                "rust-review",
                "--eval",
                "--contract",
                "contract.toml",
                "--json",
            ]),
            true,
        )
        .await;
        assert!(promoted);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["promoted"], true);
        assert_eq!(json["event_kind"], "playbook_promoted");
        assert_eq!(json["verdict_overall"], "Pass");
        assert_eq!(json["playbook"]["trust"], "verified");
        assert_eq!(json["eval_report"]["delta"]["pass_rate"], 1.0);
        let promote_session = json["promote_session"].as_str().unwrap();
        let verdict_seq = json["verdict_ref"]["seq"].as_i64().unwrap();

        let store = Store::open(
            db.to_str().unwrap(),
            Arc::new(Signer::load_or_create(key.to_str().unwrap()).unwrap()),
        )
        .unwrap();
        let memory = SqliteMemoryStore::from_store(&store).unwrap();
        let promoted_entry = find_playbook(&memory, "rust-review").unwrap();
        assert_eq!(promoted_entry.trust, Trust::Verified);
        assert_eq!(
            promoted_entry.provenance.verified_by_ref,
            Some(LedgerRef {
                session: promote_session.into(),
                seq: verdict_seq,
            })
        );
        let event = store
            .entries(promote_session)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "playbook_promoted")
            .expect("playbook eval promoted event");
        assert!(!event.signature.is_empty());
        assert_eq!(event.payload["overall"], "Pass");
        assert_eq!(event.payload["eval_report"]["delta"]["pass_rate"], 1.0);
        assert!(store.verify_chain(promote_session).unwrap().is_ok());

        std::fs::remove_file(db).ok();
        std::fs::remove_file(key).ok();
        std::fs::remove_file(source).ok();
    }

    #[tokio::test]
    async fn playbook_eval_promote_no_lift_leaves_unverified_with_signed_rejection() {
        let db = temp_db_path("playbook-eval-promote-fail");
        let key = temp_text_path("playbook-eval-promote-fail-key", "key");
        let source = temp_text_path("eval-unhelpful-skill", "md");
        std::fs::write(
            &source,
            r#"# Rust Review

## Purpose
Use the Rust Review candidate playbook for cargo fmt validation.

## Procedure
1. Say nothing useful.
"#,
        )
        .unwrap();
        run_playbook(&playbook_args(&[
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
            "playbook",
            "import",
            source.to_str().unwrap(),
            "--key",
            "rust-review",
        ]));

        let (output, promoted) = run_playbook_promote_with_eval(
            &playbook_args(&[
                "--db",
                db.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
                "playbook",
                "promote",
                "rust-review",
                "--eval",
                "--contract",
                "contract.toml",
                "--json",
            ]),
            false,
        )
        .await;
        assert!(!promoted);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["promoted"], false);
        assert_eq!(json["event_kind"], "playbook_promotion_rejected");
        assert_eq!(json["verdict_overall"], "Fail");
        assert_eq!(json["playbook"]["trust"], "unverified");
        assert_eq!(json["eval_report"]["delta"]["pass_rate"], 0.0);
        let promote_session = json["promote_session"].as_str().unwrap();

        let store = Store::open(
            db.to_str().unwrap(),
            Arc::new(Signer::load_or_create(key.to_str().unwrap()).unwrap()),
        )
        .unwrap();
        let memory = SqliteMemoryStore::from_store(&store).unwrap();
        let draft = find_playbook(&memory, "rust-review").unwrap();
        assert_eq!(draft.trust, Trust::Unverified);
        assert!(draft.provenance.verified_by_ref.is_none());
        let event = store
            .entries(promote_session)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "playbook_promotion_rejected")
            .expect("playbook eval rejection event");
        assert!(!event.signature.is_empty());
        assert_eq!(event.payload["overall"], "Fail");
        assert_eq!(event.payload["eval_report"]["delta"]["pass_rate"], 0.0);
        assert!(store.verify_chain(promote_session).unwrap().is_ok());

        std::fs::remove_file(db).ok();
        std::fs::remove_file(key).ok();
        std::fs::remove_file(source).ok();
    }

    fn temp_db_path(label: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "truss-ledger-{label}-{}-{unique}.db",
            std::process::id()
        ))
    }

    fn seed_ledger_db(path: &std::path::Path) {
        seed_ledger_db_with_overall(path, "Pass", "Pass", "Pass");
    }

    fn pass_memory_verdict() -> Verdict {
        Verdict {
            contract_id: "memory-panel".into(),
            title: "Memory panel".into(),
            overall: ClauseVerdict::Pass,
            results: vec![ClauseResult {
                kind: "review".into(),
                desc: "memory is supported".into(),
                verdict: ClauseVerdict::Pass,
                evidence: "fixture panel passed".into(),
            }],
            executor_summary: "memory fixture output".into(),
            reviewed_diff: None,
            usage: BudgetSnapshot::default(),
        }
    }

    fn seed_recall_db(path: &std::path::Path) -> String {
        let store = Store::open(path.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
        store
            .create_session("recall-run", Some("mock-model"), "local", "local")
            .unwrap();
        store
            .append(
                "recall-run",
                "run_started",
                &json!({ "prompt": "remember it" }),
            )
            .unwrap();
        let memory_write = store
            .append(
                "recall-run",
                "memory_write",
                &json!({ "kind": "lesson", "body": "Use the signed journal as recall source." }),
            )
            .unwrap();
        let memory = SqliteMemoryStore::from_store(&store).unwrap();
        let lesson_id = memory
            .write(MemoryDraft {
                kind: EntryKind::Lesson,
                key: Some("ledger-recall".into()),
                body: "Use the signed journal as recall source.".into(),
                produced_by: LedgerRef {
                    session: "recall-run".into(),
                    seq: memory_write.seq,
                },
                sources: vec![LedgerRef {
                    session: "recall-run".into(),
                    seq: memory_write.seq,
                }],
            })
            .unwrap();
        apply_memory_verdict(&memory, Some(&store), &lesson_id, pass_memory_verdict()).unwrap();
        let first = store
            .append(
                "recall-run",
                "tool_call",
                &json!({ "id": "c1", "name": "echo", "input": { "value": "first-result" } }),
            )
            .unwrap();
        let last = store
            .append(
                "recall-run",
                "tool_result",
                &json!({ "id": "c1", "name": "echo", "content": "first-result" }),
            )
            .unwrap();
        let range = LedgerRange {
            start: LedgerRef {
                session: "recall-run".into(),
                seq: first.seq,
            },
            end: LedgerRef {
                session: "recall-run".into(),
                seq: last.seq,
            },
        };
        let summary_id = memory
            .write(MemoryDraft {
                kind: EntryKind::ContextSummary,
                key: Some("recall-run:3-4".into()),
                body: "[context summary]\nelided_journal_range: recall-run#3..recall-run#4\n\nfirst tool result was elided".into(),
                produced_by: LedgerRef {
                    session: "recall-run".into(),
                    seq: last.seq,
                },
                sources: vec![range.start.clone(), range.end.clone()],
            })
            .unwrap();
        store
            .append(
                "recall-run",
                "compaction_performed",
                &json!({
                    "mode": "summarize",
                    "summary_memory_id": summary_id.0,
                    "summary_trust": "unverified",
                    "elided_range": range,
                }),
            )
            .unwrap();
        store.set_status("recall-run", "completed").unwrap();
        summary_id.0
    }

    fn seed_ledger_db_with_overall(
        path: &std::path::Path,
        overall: &str,
        check_verdict: &str,
        review_verdict: &str,
    ) {
        let store = Store::open(path.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
        store
            .create_session("run-a", Some("mock-model"), "local", "local")
            .unwrap();
        store
            .append("run-a", "run_started", &json!({ "prompt": "prove it" }))
            .unwrap();
        store
            .append(
                "run-a",
                "model_result",
                &json!({ "content": "work complete", "stop_reason": "EndTurn" }),
            )
            .unwrap();
        store
            .append(
                "run-a",
                "self_validation",
                &json!({
                    "cmd": "cargo test --workspace",
                    "exit": 0,
                    "summary": "tests passed",
                    "workdir_hash": "sha256:fixture",
                    "evidence_only": true,
                    "verdict_unchanged": true,
                }),
            )
            .unwrap();
        store
            .append(
                "run-a",
                "contract",
                &json!({
                    "id": "contract-a",
                    "title": "Fixture contract",
                    "intent": "Prove the ledger reader can render verdict evidence.",
                }),
            )
            .unwrap();
        store
            .append(
                "run-a",
                "verdict",
                &json!({
                    "overall": overall,
                    "clauses": [
                        {
                            "kind": "check",
                            "desc": "workspace tests pass",
                            "verdict": check_verdict,
                            "evidence": "$ cargo test --workspace\n[exit 0]\nok"
                        },
                        {
                            "kind": "review",
                            "desc": "the implementation satisfies LR-2",
                            "verdict": review_verdict,
                            "evidence": "panel: 2/3 reviewers say met\n  [1/mock-a] met — check evidence is complete\n  [2/mock-b] REFUTED — missing buyer wording\n  [3/mock-c] met — export is portable"
                        }
                    ]
                }),
            )
            .unwrap();
        let head = store.head_hash_hex("run-a").unwrap().unwrap();
        store
            .record_anchor(
                "run-a",
                "anchors.jsonl",
                7,
                &head,
                "anchor-entry-hash",
                1234,
            )
            .unwrap();
        store.set_status("run-a", "completed").unwrap();
    }

    fn run_ledger(args: &Args) -> String {
        let Some(Command::Ledger(ledger)) = &args.command else {
            panic!("expected ledger command");
        };
        let mut output = Vec::new();
        handle_ledger_command(args, ledger, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn run_review(args: &Args) -> String {
        let Some(Command::Review(review)) = &args.command else {
            panic!("expected review command");
        };
        let mut output = Vec::new();
        handle_review_command(args, review, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn review_command_records_signed_human_review_and_verdict_json() {
        let db = temp_db_path("review-command");
        let key = temp_text_path("review-command-key", "key");
        seed_ledger_db(&db);

        let output: serde_json::Value = serde_json::from_str(&run_review(&review_args(&[
            "review",
            "approve",
            "run-a",
            "--note",
            "ready to ship",
            "--json",
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
        ])))
        .unwrap();
        assert_eq!(output["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(output["command"], "review");
        assert_eq!(output["session_id"], "run-a");
        assert_eq!(output["decision"], "approved");
        assert_eq!(output["note"], "ready to ship");
        assert!(output["attested_head_hash"].as_str().unwrap().len() >= 64);
        assert!(output["reviewer_key_id"].as_str().unwrap().len() >= 64);

        let verdict: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "verdict",
            "run-a",
            "--json",
        ])))
        .unwrap();
        assert_eq!(verdict["verdict"]["overall"], "PASS");
        assert_eq!(verdict["verdict"]["human_signoff"]["decision"], "approved");
        assert_eq!(verdict["verdict"]["human_signoff"]["note"], "ready to ship");
        assert_eq!(
            verdict["verdict"]["human_signoff"]["attested_head_matches_current"],
            true
        );
        assert_eq!(
            verdict["verdict"]["human_signoff"]["reviewer_key_id"],
            output["reviewer_key_id"]
        );
        let verify = LedgerReader::new(Store::open_readonly(db.to_str().unwrap()).unwrap())
            .verify("run-a", None)
            .unwrap();
        assert_eq!(verify.verification.status, VerificationStatus::Verified);

        let human = run_review(&review_args(&[
            "review",
            "needs-changes",
            "run-a",
            "--note",
            "follow-up needed",
            "--db",
            db.to_str().unwrap(),
            "--key",
            key.to_str().unwrap(),
        ]));
        assert!(human.contains("decision: needs_changes"));
        assert!(human.contains("attested_head_hash:"));
        assert!(human.contains("reviewer_key_id:"));

        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn ledger_list_show_and_get_render_human_output() {
        let db = temp_db_path("human");
        seed_ledger_db(&db);

        let list = run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "list",
        ]));
        assert!(list.contains("run-a"));
        assert!(list.contains("verified"));
        assert!(list.contains("pass"));

        let show = run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "show",
            "run-a",
        ]));
        assert!(show.contains("session: run-a"));
        assert!(show.contains("run_started"));
        assert!(show.contains("verify: verified"));

        let get = run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "get",
            "run-a",
            "0",
        ]));
        assert!(get.contains("kind: run_started"));
        assert!(get.contains("\"prompt\": \"prove it\""));

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn ledger_json_outputs_stable_versioned_shapes() {
        let db = temp_db_path("json");
        seed_ledger_db(&db);

        let list: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "list",
            "--json",
        ])))
        .unwrap();
        assert_eq!(list["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(list["command"], "list");
        assert_eq!(list["sessions"][0]["id"], "run-a");
        assert_eq!(list["sessions"][0]["verification"]["status"], "verified");

        let show: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "show",
            "run-a",
            "--json",
        ])))
        .unwrap();
        assert_eq!(show["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(show["command"], "show");
        assert_eq!(show["session"]["entries"][0]["kind"], "run_started");
        assert_eq!(
            show["session"]["entries"][0]["verification_status"],
            "verified"
        );

        let get: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "get",
            "run-a",
            "0",
            "--json",
        ])))
        .unwrap();
        assert_eq!(get["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(get["command"], "get");
        assert_eq!(get["session_id"], "run-a");
        assert_eq!(get["entry"]["payload"]["prompt"], "prove it");

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn ledger_read_marks_tampered_entry_unverified_without_panicking() {
        let db = temp_db_path("tamper");
        seed_ledger_db(&db);
        {
            let store = Store::open(db.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
            store.raw_flip_signature_byte_for_test("run-a", 1).unwrap();
        }

        let show = run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "show",
            "run-a",
        ]));
        assert!(show.contains("verify: unverified (failed at seq 1)"));
        assert!(show.contains("model_result"));

        let get: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "get",
            "run-a",
            "1",
            "--json",
        ])))
        .unwrap();
        assert_eq!(get["entry"]["verification_status"], "unverified");
        assert_eq!(get["entry"]["signed"], true);

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn ledger_recall_surfaces_memory_and_recovers_elided_range() {
        let db = temp_db_path("recall");
        let summary_id = seed_recall_db(&db);

        let recall = run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "recall",
            "recall-run",
        ]));
        assert!(recall.contains("context_summary"));
        assert!(recall.contains("trust=unverified"));
        assert!(recall.contains("trust=verified"));
        assert!(recall.contains("recoverable=true"));
        assert!(recall.contains("recall-run#3..recall-run#4"));

        let expanded: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "recall",
            "recall-run",
            "--summary",
            &summary_id,
            "--json",
        ])))
        .unwrap();
        assert_eq!(expanded["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(expanded["command"], "recall");
        assert_eq!(expanded["recall"]["session_id"], "recall-run");
        assert_eq!(expanded["recall"]["verification"]["status"], "verified");
        assert_eq!(
            expanded["recall"]["memory_entries"][0]["verification_status"],
            "verified"
        );
        assert_eq!(
            expanded["recall"]["compactions"][0]["summary_memory_id"],
            summary_id
        );
        assert_eq!(
            expanded["recall"]["expanded_summary"]["entries"][1]["payload"]["content"],
            "first-result"
        );

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn ledger_verify_reports_intact_and_tampered_chains() {
        let intact_db = temp_db_path("verify-intact");
        seed_ledger_db(&intact_db);
        let verify: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            intact_db.to_str().unwrap(),
            "ledger",
            "verify",
            "run-a",
            "--json",
        ])))
        .unwrap();
        assert_eq!(verify["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(verify["command"], "verify");
        assert_eq!(verify["verify"]["verification"]["status"], "verified");
        assert_eq!(
            verify["verify"]["entries"][0]["verification_status"],
            "verified"
        );

        let tampered_db = temp_db_path("verify-tamper");
        seed_ledger_db(&tampered_db);
        {
            let store =
                Store::open(tampered_db.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
            store.raw_flip_signature_byte_for_test("run-a", 1).unwrap();
        }
        let tampered = run_ledger(&ledger_args(&[
            "--db",
            tampered_db.to_str().unwrap(),
            "ledger",
            "verify",
            "run-a",
        ]));
        assert!(tampered.contains("TAMPER: verification failed at seq 1"));
        assert!(tampered.contains("model_result"));
        assert!(tampered.contains("unverified"));

        let _ = std::fs::remove_file(intact_db);
        let _ = std::fs::remove_file(tampered_db);
    }

    #[test]
    fn ledger_recall_marks_tampered_memory_provenance_unverified() {
        let db = temp_db_path("recall-tamper");
        seed_recall_db(&db);
        {
            let store = Store::open(db.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
            store
                .raw_flip_signature_byte_for_test("recall-run", 3)
                .unwrap();
        }

        let recall: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "recall",
            "recall-run",
            "--json",
        ])))
        .unwrap();
        assert_eq!(recall["recall"]["verification"]["status"], "unverified");
        assert!(recall["recall"]["memory_entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "context_summary"
                && entry["verification_status"] == "unverified"));
        assert_eq!(
            recall["recall"]["compactions"][0]["entry"]["verification_status"],
            "unverified"
        );

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn ledger_verdict_renders_pass_fail_and_unverifiable_bundles() {
        let pass_db = temp_db_path("verdict-pass");
        seed_ledger_db_with_overall(&pass_db, "Pass", "Pass", "Pass");
        let pass = run_ledger(&ledger_args(&[
            "--db",
            pass_db.to_str().unwrap(),
            "ledger",
            "verdict",
            "run-a",
        ]));
        assert!(pass.contains("overall: PASS"));
        assert!(pass.contains("definition_of_done:"));
        assert!(pass.contains("checks:"));
        assert!(pass.contains("workspace tests pass"));
        assert!(pass.contains("panel:"));
        assert!(pass.contains("majority: PASS (2/3)"));
        assert!(pass.contains("self_validation:"));

        let fail_db = temp_db_path("verdict-fail");
        seed_ledger_db_with_overall(&fail_db, "Fail", "Fail", "Fail");
        let fail: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            fail_db.to_str().unwrap(),
            "ledger",
            "verdict",
            "run-a",
            "--json",
        ])))
        .unwrap();
        assert_eq!(fail["schema_version"], LEDGER_SCHEMA_VERSION);
        assert_eq!(fail["command"], "verdict");
        assert_eq!(fail["verdict"]["overall"], "FAIL");
        assert_eq!(fail["verdict"]["evidence"]["checks"][0]["verdict"], "FAIL");

        let unverifiable_db = temp_db_path("verdict-unverifiable");
        seed_ledger_db_with_overall(&unverifiable_db, "Unverifiable", "Pass", "Unverifiable");
        let unverifiable: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            unverifiable_db.to_str().unwrap(),
            "ledger",
            "verdict",
            "run-a",
            "--json",
        ])))
        .unwrap();
        assert_eq!(unverifiable["verdict"]["overall"], "UNVERIFIABLE");
        assert_eq!(
            unverifiable["verdict"]["evidence"]["panel"][0]["clause_verdict"],
            "UNVERIFIABLE"
        );

        let _ = std::fs::remove_file(pass_db);
        let _ = std::fs::remove_file(fail_db);
        let _ = std::fs::remove_file(unverifiable_db);
    }

    #[test]
    fn ledger_export_is_self_contained_and_offline_verifiable() {
        let db = temp_db_path("export");
        seed_ledger_db(&db);

        let export: truss_core::ledger::LedgerExportBundle =
            serde_json::from_str(&run_ledger(&ledger_args(&[
                "--db",
                db.to_str().unwrap(),
                "ledger",
                "export",
                "run-a",
            ])))
            .unwrap();
        assert_eq!(export.schema_version, LEDGER_SCHEMA_VERSION);
        assert_eq!(export.command, "export");
        assert_eq!(export.verdict.overall, "PASS");
        assert!(!export.ledger.entries.is_empty());
        assert!(!export.ledger.anchor_refs.is_empty());
        assert_eq!(
            export.offline_verification.status,
            truss_core::ledger::VerificationStatus::Verified
        );
        assert_eq!(
            truss_core::ledger::verify_export_bundle(&export).status,
            truss_core::ledger::VerificationStatus::Verified
        );

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn ledger_verdict_and_export_mark_tampered_chain_unverified() {
        let db = temp_db_path("verdict-tamper");
        seed_ledger_db(&db);
        {
            let store = Store::open(db.to_str().unwrap(), Arc::new(Signer::generate())).unwrap();
            store.raw_flip_signature_byte_for_test("run-a", 2).unwrap();
        }

        let verdict = run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "verdict",
            "run-a",
        ]));
        assert!(verdict.contains("verify: unverified (failed at seq 2)"));
        assert!(verdict.contains("overall: UNVERIFIABLE"));
        assert!(verdict.contains("recorded_overall: PASS (untrusted)"));

        let export: serde_json::Value = serde_json::from_str(&run_ledger(&ledger_args(&[
            "--db",
            db.to_str().unwrap(),
            "ledger",
            "export",
            "run-a",
        ])))
        .unwrap();
        assert_eq!(
            export["verdict"]["verification"]["status"],
            serde_json::Value::String("unverified".into())
        );
        assert_eq!(
            export["offline_verification"]["status"],
            serde_json::Value::String("unverified".into())
        );

        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn memory_compaction_toml_parses_and_layers_global_agent_contract() {
        let config = config_from(
            r#"
            [provider]
            kind = "openai"
            model = "gpt-5"

            [memory]
            verification = "off"

            [memory.compaction]
            mode = "summarize"
            threshold = 0.60
            min_tail = 4
            anticipate = false

            [memory.effective_budget]
            ratio = 0.50

            [memory.effective_budget.overrides]
            "claude-opus-4-8" = 250000

            [agent.memory.compaction]
            threshold = 0.75
            anticipate = true

            [agent.memory.effective_budget]
            ratio = 0.55

            [agent.memory.effective_budget.overrides]
            "gpt-5" = 128000
            "#,
        );
        let mut contract_overrides = std::collections::BTreeMap::new();
        contract_overrides.insert("gpt-5".to_string(), 96000);
        let contract = MemoryConfigLayer {
            verification: Some(VerificationPolicy::Lazy),
            compaction: Some(CompactionPolicyLayer {
                mode: Some(CompactionMode::Evict),
                threshold: Some(0.90),
                min_tail: Some(2),
                anticipate: Some(false),
                ..CompactionPolicyLayer::default()
            }),
            effective_budget: Some(EffectiveBudgetPolicyLayer {
                ratio: Some(0.40),
                overrides: Some(contract_overrides),
            }),
            ..MemoryConfigLayer::default()
        };
        let args = args_from(&[]);

        let resolved = resolved_memory_config(Some(&config), Some(&contract), &args);

        assert_eq!(resolved.verification, VerificationPolicy::Lazy);
        assert_eq!(resolved.compaction.mode, CompactionMode::Evict);
        assert_eq!(resolved.compaction.threshold, 0.90);
        assert_eq!(resolved.compaction.min_tail, 2);
        assert!(!resolved.compaction.anticipate);
        assert_eq!(resolved.effective_budget.ratio, 0.40);
        assert_eq!(
            resolved.effective_budget.overrides.get("claude-opus-4-8"),
            Some(&250000)
        );
        assert_eq!(
            resolved.effective_budget.overrides.get("gpt-5"),
            Some(&96000)
        );

        let inherited = resolved_memory_config(Some(&config), None, &args);
        assert_eq!(inherited.compaction.mode, CompactionMode::Summarize);
        assert_eq!(inherited.compaction.threshold, 0.75);
        assert_eq!(inherited.compaction.min_tail, 4);
        assert!(inherited.compaction.anticipate);
        assert_eq!(inherited.effective_budget.ratio, 0.55);
        assert_eq!(
            inherited.effective_budget.overrides.get("gpt-5"),
            Some(&128000)
        );
    }

    #[test]
    fn ai_memory_backend_toml_is_explicit_and_layered() {
        let config = config_from(
            r#"
            [provider]
            kind = "openai"
            model = "gpt-5"

            [memory]
            store_backend = "ai_memory"
            ai_memory_socket = "/tmp/truss-ai-memory.sock"
            recall = "exact"
            "#,
        );

        let resolved = resolved_memory_config(Some(&config), None, &args_from(&[]));
        assert_eq!(resolved.store_backend, MemoryStoreBackend::AiMemory);
        assert_eq!(
            resolved.ai_memory_socket.as_deref(),
            Some(std::path::Path::new("/tmp/truss-ai-memory.sock"))
        );
    }

    #[test]
    fn compaction_flags_override_config_and_defaults_stay_off() {
        let config = config_from(
            r#"
            [provider]
            kind = "openai"
            model = "gpt-5"

            [memory.compaction]
            mode = "evict"
            threshold = 0.80
            min_tail = 9
            anticipate = false
            "#,
        );
        let args = args_from(&[
            "--compaction-mode",
            "summarize",
            "--compaction-threshold",
            "0.50",
            "--compaction-min-tail",
            "1",
            "--compaction-anticipate",
            "--effective-budget-ratio",
            "0.40",
            "--effective-budget-override",
            "gpt-5=123456",
        ]);

        let resolved = resolved_memory_config(Some(&config), None, &args);
        assert_eq!(resolved.compaction.mode, CompactionMode::Summarize);
        assert_eq!(resolved.compaction.threshold, 0.50);
        assert_eq!(resolved.compaction.min_tail, 1);
        assert!(resolved.compaction.anticipate);
        assert_eq!(resolved.effective_budget.ratio, 0.40);
        assert_eq!(
            resolved.effective_budget.overrides.get("gpt-5"),
            Some(&123456)
        );

        let config_only = resolved_memory_config(Some(&config), None, &args_from(&[]));
        assert_eq!(config_only.compaction.mode, CompactionMode::Evict);
        assert_eq!(config_only.compaction.threshold, 0.80);
        assert_eq!(config_only.compaction.min_tail, 9);
        assert!(!config_only.compaction.anticipate);

        let defaulted = resolved_memory_config(None, None, &args_from(&[]));
        assert_eq!(defaulted.compaction.mode, CompactionMode::Off);
        assert_eq!(defaulted.compaction.threshold, 0.70);
        assert_eq!(defaulted.compaction.min_tail, 6);
        assert!(!defaulted.compaction.anticipate);
        assert_eq!(defaulted.effective_budget.ratio, 0.60);
        assert!(defaulted.effective_budget.overrides.is_empty());
    }

    #[test]
    fn effective_budget_flags_reject_out_of_range_values() {
        let err = Args::try_parse_from(["truss", "--effective-budget-ratio", "1.01", "prompt"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("effective budget ratio must be between 0.0 and 1.0"));

        let err =
            Args::try_parse_from(["truss", "--effective-budget-override", "gpt-5=0", "prompt"])
                .unwrap_err()
                .to_string();
        assert!(err.contains("token budget must be greater than zero"));
    }

    #[test]
    fn sandbox_toml_parses_and_layers_global_agent_contract_default_off() {
        let config = config_from(
            r#"
            [provider]
            kind = "openai"
            model = "gpt-5"

            [sandbox]
            mode = "working_copy"
            checkout = "/tmp/global"

            [agent.sandbox]
            checkout = "/tmp/agent"
            "#,
        );
        let contract = SandboxConfigLayer {
            mode: Some(SandboxMode::NoSandbox),
            ..SandboxConfigLayer::default()
        };

        let resolved = resolved_sandbox_config(Some(&config), Some(&contract));
        assert_eq!(resolved.mode, SandboxMode::NoSandbox);
        assert_eq!(
            resolved.checkout.as_deref(),
            Some(std::path::Path::new("/tmp/agent"))
        );

        let inherited = resolved_sandbox_config(Some(&config), None);
        assert_eq!(inherited.mode, SandboxMode::WorkingCopy);
        assert_eq!(
            inherited.checkout.as_deref(),
            Some(std::path::Path::new("/tmp/agent"))
        );

        assert_eq!(
            resolved_sandbox_config(None, None).mode,
            SandboxMode::NoSandbox
        );
    }

    #[test]
    fn compaction_threshold_flag_rejects_out_of_range_values() {
        let err = Args::try_parse_from(["truss", "--compaction-threshold", "1.01", "prompt"])
            .expect_err("threshold > 1 should be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);

        let err = Args::try_parse_from(["truss", "--compaction-threshold=-0.01", "prompt"])
            .expect_err("threshold < 0 should be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[tokio::test]
    async fn compaction_mode_flag_reaches_durable_compaction_path() {
        let args = args_from(&[
            "--compaction-mode",
            "summarize",
            "--compaction-threshold",
            "0.70",
            "--compaction-min-tail",
            "1",
        ]);
        let memory = resolved_memory_config(None, None, &args);
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            calls: Arc::new(Mutex::new(0)),
            script: vec![
                tool_use("c1", "first-result", 10),
                tool_use("c2", "second-result", 80),
                text("done", 10),
            ],
            context_window: 100,
        });
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        let store = Store::open_memory().unwrap();
        let memory_store = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());

        let outcome = durable::run(
            provider,
            Arc::new(tools),
            &Ctx::new(),
            &store,
            "cli-compaction-smoke",
            &DurableConfig {
                max_steps: 8,
                memory,
                memory_store: Some(memory_store.clone()),
                ..DurableConfig::default()
            },
            Some("original task".into()),
        )
        .await
        .expect("durable run succeeds");

        assert!(matches!(outcome, DurableOutcome::Completed { .. }));
        let entries = store.entries("cli-compaction-smoke").unwrap();
        let compaction = entries
            .iter()
            .find(|entry| entry.kind == "compaction_performed")
            .expect("--compaction-mode summarize triggers governed compaction");
        assert_eq!(compaction.payload["mode"], "summarize");
        let summary_id = compaction.payload["summary_memory_id"].as_str().unwrap();
        assert!(memory_store
            .get(&truss_core::memory::MemoryId(summary_id.to_string()))
            .unwrap()
            .body
            .contains("earlier turns elided"));
    }
}

struct StoreToolOptions {
    memory_tool: bool,
    memory_tools: bool,
    memory_config: MemoryConfig,
    memory_reviewers: Vec<Arc<dyn Provider>>,
    spawn_tool: bool,
    provider: Arc<dyn Provider>,
    spawn_cfg: DurableConfig,
    self_check: bool,
    session_id: String,
    declared_test_command: Option<String>,
    mcp_servers: Vec<McpServerConfig>,
    git_tools: bool,
    github_tools: bool,
    web_tools: bool,
    web_search_config: truss_core::tools::WebSearchConfig,
}

async fn tools_for_store(
    tools: &Arc<ToolRegistry>,
    store: &Store,
    ctx: &Ctx,
    options: StoreToolOptions,
) -> Result<Arc<ToolRegistry>> {
    if !options.memory_tool
        && !options.memory_tools
        && !options.spawn_tool
        && !options.self_check
        && options.mcp_servers.is_empty()
        && !options.git_tools
        && !options.github_tools
        && !options.web_tools
    {
        return Ok(tools.clone());
    }
    let mut registry = (**tools).clone();
    if options.web_tools {
        registry.register_web_tools(
            Some(truss_core::tools::WebJournal {
                store: store.clone(),
                session_id: options.session_id.clone(),
            }),
            options.web_search_config,
        );
    }
    if options.memory_tool {
        let memory_store = memory_store_from_config(store, &options.memory_config)
            .map_err(|e| anyhow!(e.to_string()))?;
        registry.register_memory_with_signed_promotion(
            memory_store,
            options.memory_config.clone(),
            options.memory_reviewers,
            store.clone(),
        );
    }
    if options.memory_tools {
        let memory_store = memory_store_from_config(store, &options.memory_config)
            .map_err(|e| anyhow!(e.to_string()))?;
        registry.register_memory_read_tools(
            memory_store,
            Some(truss_core::tools::MemoryReadJournal {
                store: store.clone(),
                session_id: options.session_id.clone(),
            }),
            options.memory_config.recall,
        );
    }
    if options.spawn_tool {
        let child_tools = Arc::new(registry.clone());
        registry.register_spawn(
            options.provider,
            child_tools,
            store.clone(),
            options.spawn_cfg,
        );
    }
    if options.self_check {
        registry.register_self_check(
            store.clone(),
            options.session_id.clone(),
            options.declared_test_command,
        );
    }
    if !options.mcp_servers.is_empty() {
        registry
            .register_mcp_servers(
                ctx,
                &options.mcp_servers,
                Some(McpJournal {
                    store: store.clone(),
                    session_id: options.session_id.clone(),
                }),
            )
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
    }
    if options.git_tools {
        registry.register_git_tools(Some(truss_core::tools::GitJournal {
            store: store.clone(),
            session_id: options.session_id.clone(),
        }));
    }
    if options.github_tools {
        registry.register_github_tools(Some(truss_core::tools::GitHubJournal {
            store: store.clone(),
            session_id: options.session_id,
        }));
    }
    Ok(Arc::new(registry))
}

fn register_web_tools_without_journal(
    registry: &mut ToolRegistry,
    args: &Args,
    config: Option<&Config>,
) {
    if web_tools_enabled(args, config) {
        registry.register_web_tools(None, web_search_config(config));
    }
}

fn web_tools_enabled(args: &Args, config: Option<&Config>) -> bool {
    args.web_tools || config.map(|c| c.tools.web).unwrap_or(false)
}

fn web_search_config(config: Option<&Config>) -> truss_core::tools::WebSearchConfig {
    config
        .map(|c| c.tools.web_search.clone().into())
        .unwrap_or_default()
}

fn register_memory_tools_without_journal(
    registry: &mut ToolRegistry,
    args: &Args,
    config: Option<&Config>,
    memory_config: &MemoryConfig,
) -> Result<()> {
    if !memory_tools_enabled(args, config) {
        return Ok(());
    }
    let signer = Arc::new(Signer::load_or_create(&args.key).map_err(|e| anyhow!(e.to_string()))?);
    let store = Store::open(&args.db, signer).map_err(|e| anyhow!(e.to_string()))?;
    let memory_store = memory_store_from_config(&store, memory_config)?;
    registry.register_memory_read_tools(memory_store, None, memory_config.recall);
    Ok(())
}

fn memory_tools_enabled(args: &Args, config: Option<&Config>) -> bool {
    args.memory_tools || config.map(|c| c.tools.memory).unwrap_or(false)
}

fn mcp_servers_for(
    config: Option<&Config>,
    contract_mcp: Option<&truss_core::mcp::McpConfig>,
) -> Vec<McpServerConfig> {
    let mut servers = config.map(|c| c.mcp.servers.clone()).unwrap_or_default();
    if let Some(mcp) = contract_mcp {
        servers.extend(mcp.servers.clone());
    }
    servers
}

fn register_native_tools_without_journal(
    registry: &mut ToolRegistry,
    args: &Args,
    config: Option<&Config>,
) {
    if git_tools_enabled(args, config) {
        registry.register_git_tools(None);
    }
    if github_tools_enabled(args, config) {
        registry.register_github_tools(None);
    }
}

fn git_tools_enabled(args: &Args, config: Option<&Config>) -> bool {
    args.git_tools || config.map(|c| c.tools.git).unwrap_or(false)
}

fn github_tools_enabled(args: &Args, config: Option<&Config>) -> bool {
    args.github_tools || config.map(|c| c.tools.github).unwrap_or(false)
}

struct ServeMcpRuntime<'a> {
    serve: &'a ServeMcpArgs,
    config: Option<Arc<Config>>,
    provider: Arc<dyn Provider>,
    memory_reviewer: Arc<dyn Provider>,
    memory_reviewers: Vec<Arc<dyn Provider>>,
    tools: Arc<ToolRegistry>,
    args: ServeMcpSettings,
}

#[derive(Clone)]
struct ServeMcpSettings {
    db: String,
    key: String,
    model: Option<String>,
    max_tokens: u32,
    max_steps: usize,
    reasoning: Option<ReasoningEffort>,
    temperature: Option<f32>,
    max_context_msgs: usize,
    timeout: Option<u64>,
    memory_tool: bool,
    memory_tools: bool,
    spawn_tool: bool,
    git_tools: bool,
    github_tools: bool,
    web_tools: bool,
    web_search_config: truss_core::tools::WebSearchConfig,
    reviewers: Option<usize>,
    workdir: Option<String>,
    compaction_mode: Option<CompactionModeArg>,
    compaction_threshold: Option<f64>,
    compaction_min_tail: Option<usize>,
    compaction_anticipate: bool,
    effective_budget_ratio: Option<f64>,
    effective_budget_override: Vec<EffectiveBudgetOverrideArg>,
}

impl ServeMcpSettings {
    #[allow(clippy::too_many_arguments)]
    fn from_args(
        args: &Args,
        max_tokens: u32,
        max_steps: usize,
        reasoning: Option<ReasoningEffort>,
        temperature: Option<f32>,
        memory_tools: bool,
        git_tools: bool,
        github_tools: bool,
        web_tools: bool,
        web_search_config: truss_core::tools::WebSearchConfig,
    ) -> Self {
        Self {
            db: args.db.clone(),
            key: args.key.clone(),
            model: args.model.clone(),
            max_tokens,
            max_steps,
            reasoning,
            temperature,
            max_context_msgs: args.max_context_msgs,
            timeout: args.timeout,
            memory_tool: args.memory_tool,
            memory_tools,
            spawn_tool: args.spawn_tool,
            git_tools,
            github_tools,
            web_tools,
            web_search_config,
            reviewers: args.reviewers,
            workdir: args.workdir.clone(),
            compaction_mode: args.compaction_mode,
            compaction_threshold: args.compaction_threshold,
            compaction_min_tail: args.compaction_min_tail,
            compaction_anticipate: args.compaction_anticipate,
            effective_budget_ratio: args.effective_budget_ratio,
            effective_budget_override: args.effective_budget_override.clone(),
        }
    }

    fn apply_memory_overrides(&self, memory: &mut MemoryConfig) {
        if let Some(mode) = self.compaction_mode {
            memory.compaction.mode = mode.into();
        }
        if let Some(threshold) = self.compaction_threshold {
            memory.compaction.threshold = threshold;
        }
        if let Some(min_tail) = self.compaction_min_tail {
            memory.compaction.min_tail = min_tail;
        }
        if self.compaction_anticipate {
            memory.compaction.anticipate = true;
        }
        if let Some(ratio) = self.effective_budget_ratio {
            memory.effective_budget.ratio = ratio;
        }
        for override_arg in &self.effective_budget_override {
            memory
                .effective_budget
                .overrides
                .insert(override_arg.model.clone(), override_arg.budget);
        }
    }
}

#[derive(Clone)]
struct CliMcpBackend {
    config: Option<Arc<Config>>,
    provider: Arc<dyn Provider>,
    reviewer: Arc<dyn Provider>,
    memory_reviewers: Vec<Arc<dyn Provider>>,
    tools: Arc<ToolRegistry>,
    settings: ServeMcpSettings,
    jobs: Arc<Mutex<HashMap<String, ContractStatusResponse>>>,
}

async fn handle_serve_mcp_command(runtime: ServeMcpRuntime<'_>) -> Result<()> {
    let principal = truss_core::ctx::Principal {
        tenant: runtime.serve.principal_tenant.clone(),
        user: runtime.serve.principal_user.clone(),
    };
    let backend = Arc::new(CliMcpBackend {
        config: runtime.config,
        provider: runtime.provider,
        reviewer: runtime.memory_reviewer,
        memory_reviewers: runtime.memory_reviewers,
        tools: runtime.tools,
        settings: runtime.args,
        jobs: Arc::new(Mutex::new(HashMap::new())),
    });
    let server = TrussMcpServer::new(backend, principal);
    server
        .serve_stdio(StdioTransport {
            stdin: tokio::io::stdin(),
            stdout: tokio::io::stdout(),
        })
        .await
        .map_err(|e| anyhow!(e))
}

struct StdioTransport {
    stdin: tokio::io::Stdin,
    stdout: tokio::io::Stdout,
}

impl tokio::io::AsyncRead for StdioTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for StdioTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdout).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_shutdown(cx)
    }
}

#[async_trait]
impl TrussMcpBackend for CliMcpBackend {
    async fn run_contract(
        &self,
        principal: &truss_core::ctx::Principal,
        request: RunContractRequest,
    ) -> std::result::Result<RunContractResponse, McpServerError> {
        if request.async_run {
            let session_id = self.session_id_for_request(&request)?;
            let response = ContractStatusResponse {
                handle: session_id.clone(),
                session_id: session_id.clone(),
                status: "running".to_string(),
                signed_verdict: None,
                head_hash: None,
                bundle_ref: Some(LedgerBundleRef::export(session_id.clone())),
                error: None,
            };
            self.jobs
                .lock()
                .map_err(|_| McpServerError::internal("contract job table poisoned"))?
                .insert(session_id.clone(), response.clone());
            let backend = self.clone();
            let principal = principal.clone();
            let mut sync_request = request.clone();
            sync_request.async_run = false;
            tokio::spawn(async move {
                let status = match backend.run_contract_now(&principal, sync_request).await {
                    Ok(done) => ContractStatusResponse {
                        handle: done.handle,
                        session_id: done.session_id,
                        status: done.status,
                        signed_verdict: done.signed_verdict,
                        head_hash: done.head_hash,
                        bundle_ref: done.bundle_ref,
                        error: None,
                    },
                    Err(err) => ContractStatusResponse {
                        handle: session_id.clone(),
                        session_id: session_id.clone(),
                        status: "failed".to_string(),
                        signed_verdict: None,
                        head_hash: None,
                        bundle_ref: Some(LedgerBundleRef::export(session_id.clone())),
                        error: Some(err.message),
                    },
                };
                if let Ok(mut jobs) = backend.jobs.lock() {
                    jobs.insert(session_id, status);
                }
            });
            Ok(RunContractResponse {
                handle: response.handle,
                session_id: response.session_id,
                status: "running".to_string(),
                verdict: None,
                signed_verdict: None,
                head_hash: None,
                bundle_ref: response.bundle_ref,
            })
        } else {
            self.run_contract_now(principal, request).await
        }
    }

    async fn contract_status(
        &self,
        _principal: &truss_core::ctx::Principal,
        request: ContractStatusRequest,
    ) -> std::result::Result<ContractStatusResponse, McpServerError> {
        if let Some(status) = self
            .jobs
            .lock()
            .map_err(|_| McpServerError::internal("contract job table poisoned"))?
            .get(&request.handle)
            .cloned()
        {
            return Ok(status);
        }
        self.status_from_store(&request.handle)
    }

    async fn ledger_verdict(
        &self,
        _principal: &truss_core::ctx::Principal,
        request: LedgerReadRequest,
    ) -> std::result::Result<Value, McpServerError> {
        let store = Store::open_readonly(&self.settings.db)
            .map_err(|e| McpServerError::internal(format!("open ledger store: {e}")))?;
        let verdict = LedgerReader::new(store)
            .verdict(&request.session_id)
            .map_err(|e| McpServerError::invalid_input(format!("read verdict bundle: {e}")))?;
        serde_json::to_value(verdict)
            .map_err(|e| McpServerError::internal(format!("encode verdict bundle: {e}")))
    }

    async fn ledger_export(
        &self,
        _principal: &truss_core::ctx::Principal,
        request: LedgerReadRequest,
    ) -> std::result::Result<Value, McpServerError> {
        let store = Store::open_readonly(&self.settings.db)
            .map_err(|e| McpServerError::internal(format!("open ledger store: {e}")))?;
        let export = LedgerReader::new(store)
            .export(&request.session_id)
            .map_err(|e| McpServerError::invalid_input(format!("export ledger bundle: {e}")))?;
        serde_json::to_value(export)
            .map_err(|e| McpServerError::internal(format!("encode ledger export: {e}")))
    }
}

impl CliMcpBackend {
    fn session_id_for_request(
        &self,
        request: &RunContractRequest,
    ) -> Result<String, McpServerError> {
        if let Some(session_id) = &request.session_id {
            return Ok(session_id.clone());
        }
        let contract = Contract::from_path(&request.contract_path)
            .map_err(|e| McpServerError::invalid_input(format!("read contract: {e}")))?;
        Ok(contract.contract.id)
    }

    async fn run_contract_now(
        &self,
        principal: &truss_core::ctx::Principal,
        request: RunContractRequest,
    ) -> std::result::Result<RunContractResponse, McpServerError> {
        let contract = Contract::from_path(&request.contract_path)
            .map_err(|e| McpServerError::invalid_input(format!("read contract: {e}")))?;
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| contract.contract.id.clone());
        let contract_memory_config = self.resolved_memory_config(Some(&contract.memory));
        let contract_approval_gates = self
            .config
            .as_ref()
            .map(|c| c.approval_gate_policy(Some(&contract.approval_gates)))
            .unwrap_or_else(|| {
                ApprovalGatePolicy::resolve(None, None, Some(&contract.approval_gates))
            });
        let mut contract_policy = contract.policy();
        let contract_sandbox_config = self
            .config
            .as_ref()
            .map(|c| c.sandbox_config(Some(&contract.sandbox)))
            .unwrap_or_else(|| SandboxConfig::resolve(None, None, Some(&contract.sandbox)));
        if contract_sandbox_config.mode == SandboxMode::WorkingCopy {
            allow_self_validation_tools(&mut contract_policy);
        }
        let contract_sandbox = sandbox_from_config(
            &contract_sandbox_config,
            default_checkout_from_workdir(self.settings.workdir.as_deref(), &contract_policy),
        );
        let signer = Arc::new(
            Signer::load_or_create(&self.settings.key)
                .map_err(|e| McpServerError::internal(format!("load signing key: {e}")))?,
        );
        let store = Store::open(&self.settings.db, signer)
            .map_err(|e| McpServerError::internal(format!("open ledger store: {e}")))?;
        let mut contract_ctx = Ctx::new()
            .with_principal(principal.clone())
            .with_policy(contract_policy)
            .with_sandbox(contract_sandbox)
            .with_budget(BudgetMeter::new(contract.budget_limits()));
        if let Some(secs) = self.settings.timeout {
            contract_ctx = contract_ctx.with_timeout(std::time::Duration::from_secs(secs));
        }
        let panel_size = self
            .settings
            .reviewers
            .or(contract.reviewers)
            .unwrap_or(1)
            .max(1);
        let reviewers: Vec<Arc<dyn Provider>> =
            (0..panel_size).map(|_| self.reviewer.clone()).collect();
        let contract_memory_store = memory_store_from_config(&store, &contract_memory_config)
            .map_err(|e| McpServerError::internal(format!("open memory store: {e}")))?;
        let exec_cfg = DurableConfig {
            system: contract.constraints.system.clone(),
            model: self.settings.model.clone(),
            max_tokens: self.settings.max_tokens,
            max_steps: contract
                .budget
                .max_steps
                .map(|n| n as usize)
                .unwrap_or(self.settings.max_steps),
            retry: RetryPolicy::default(),
            reasoning: self.settings.reasoning,
            temperature: self.settings.temperature,
            max_context_msgs: self
                .settings
                .max_context_msgs
                .max(contract_memory_config.projection.keep_tail),
            memory_tool: self.settings.memory_tool,
            memory: contract_memory_config.clone(),
            persona: None,
            principal: Some(format!("{}/{}", principal.tenant, principal.user)),
            goal_ledger: Some(truss_core::memory::GoalLedger::from_contract(
                contract.contract.intent.clone(),
                contract.done.iter().map(|d| d.desc.clone()).collect(),
                contract_memory_config.goal_repin_every,
            )),
            memory_cursor: truss_core::memory::MemoryCursor(0),
            memory_store: Some(contract_memory_store.clone()),
            approval_gates: contract_approval_gates,
        };
        let executor = match contract
            .cli_executor()
            .map_err(|e| McpServerError::invalid_input(format!("build executor: {e}")))?
        {
            Some(cli) => Executor::DelegatedCli(cli),
            None => Executor::InLoop(self.provider.clone()),
        };
        let contract_tools = tools_for_store(
            &self.tools,
            &store,
            &contract_ctx,
            StoreToolOptions {
                memory_tool: self.settings.memory_tool,
                memory_tools: self.settings.memory_tools,
                memory_config: contract_memory_config.clone(),
                memory_reviewers: self.memory_reviewers.clone(),
                spawn_tool: self.settings.spawn_tool,
                provider: self.provider.clone(),
                spawn_cfg: exec_cfg.clone(),
                self_check: contract_sandbox_config.mode == SandboxMode::WorkingCopy,
                session_id: session_id.clone(),
                declared_test_command: declared_test_command(&contract),
                mcp_servers: mcp_servers_for(self.config.as_deref(), Some(&contract.mcp)),
                git_tools: self.settings.git_tools,
                github_tools: self.settings.github_tools,
                web_tools: self.settings.web_tools,
                web_search_config: self.settings.web_search_config.clone(),
            },
        )
        .await
        .map_err(|e| McpServerError::internal(format!("register contract tools: {e}")))?;
        let close_learning = if contract_memory_config.extraction
            == truss_core::memory::LessonExtractionPolicy::Off
        {
            None
        } else {
            Some(delegation::ContractCloseLearning {
                memory_store: contract_memory_store,
                extractor: self.provider.clone(),
            })
        };
        if session_id != contract.contract.id
            && store
                .get_session(&session_id)
                .map_err(|e| McpServerError::internal(format!("read session: {e}")))?
                .is_none()
        {
            store
                .create_session(
                    &session_id,
                    Some(
                        &self
                            .settings
                            .model
                            .clone()
                            .unwrap_or_else(|| self.provider.default_model().to_string()),
                    ),
                    &principal.tenant,
                    &principal.user,
                )
                .map_err(|e| McpServerError::internal(format!("create session: {e}")))?;
        }
        let verdict = if session_id == contract.contract.id {
            delegation::run_contract_with_close_learning(
                executor,
                reviewers,
                contract_tools,
                &contract_ctx,
                &store,
                &contract,
                &exec_cfg,
                close_learning,
            )
            .await
        } else {
            delegation::run_contract_in_existing_session_with_close_learning(
                executor,
                reviewers,
                contract_tools,
                &contract_ctx,
                &store,
                &session_id,
                &contract,
                &exec_cfg,
                close_learning,
            )
            .await
        }
        .map_err(|e| McpServerError::internal(format!("run contract: {e}")))?;
        let reader = LedgerReader::new(store.clone());
        let signed_verdict = reader
            .verdict(&session_id)
            .map_err(|e| McpServerError::internal(format!("read signed verdict: {e}")))?;
        let head_hash = reader
            .head_hash_hex(&session_id)
            .map_err(|e| McpServerError::internal(format!("read head hash: {e}")))?;
        Ok(RunContractResponse {
            handle: session_id.clone(),
            session_id: session_id.clone(),
            status: "completed".to_string(),
            verdict: Some(
                serde_json::to_value(verdict)
                    .map_err(|e| McpServerError::internal(format!("encode verdict: {e}")))?,
            ),
            signed_verdict: Some(serde_json::to_value(signed_verdict).map_err(|e| {
                McpServerError::internal(format!("encode signed verdict bundle: {e}"))
            })?),
            head_hash,
            bundle_ref: Some(LedgerBundleRef::export(session_id)),
        })
    }

    fn status_from_store(
        &self,
        session_id: &str,
    ) -> std::result::Result<ContractStatusResponse, McpServerError> {
        let store = Store::open_readonly(&self.settings.db)
            .map_err(|e| McpServerError::internal(format!("open ledger store: {e}")))?;
        let reader = LedgerReader::new(store);
        let Some(session) = reader
            .get_session(session_id)
            .map_err(|e| McpServerError::internal(format!("read session: {e}")))?
        else {
            return Ok(ContractStatusResponse {
                handle: session_id.to_string(),
                session_id: session_id.to_string(),
                status: "not_found".to_string(),
                signed_verdict: None,
                head_hash: None,
                bundle_ref: Some(LedgerBundleRef::export(session_id)),
                error: None,
            });
        };
        let signed_verdict = reader
            .verdict(session_id)
            .ok()
            .and_then(|verdict| serde_json::to_value(verdict).ok());
        Ok(ContractStatusResponse {
            handle: session_id.to_string(),
            session_id: session_id.to_string(),
            status: session.status,
            signed_verdict,
            head_hash: session.head_hash.map(hex::encode),
            bundle_ref: Some(LedgerBundleRef::export(session_id)),
            error: None,
        })
    }

    fn resolved_memory_config(&self, contract: Option<&MemoryConfigLayer>) -> MemoryConfig {
        let mut memory = self
            .config
            .as_ref()
            .map(|c| c.memory_config(contract))
            .unwrap_or_else(|| MemoryConfig::resolve(None, None, contract));
        self.settings.apply_memory_overrides(&mut memory);
        memory
    }
}

fn default_checkout_from_workdir(workdir: Option<&str>, policy: &Policy) -> std::path::PathBuf {
    workdir
        .map(std::path::PathBuf::from)
        .or_else(|| policy.write_scope.first().cloned())
        .or_else(|| {
            policy
                .read_scope
                .as_ref()
                .and_then(|scopes| scopes.first().cloned())
        })
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

fn print_event(event: &AgentEvent) {
    match event {
        AgentEvent::AssistantTextDelta { text, .. } => {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::AssistantText { text, .. } if !text.is_empty() => println!(),
        AgentEvent::ToolUseRequested { name, input, .. } => {
            eprintln!("[tool] {name}({input})");
        }
        AgentEvent::ToolResult {
            name,
            is_error,
            preview,
            ..
        } => {
            let tag = if *is_error { "tool-error" } else { "tool-ok" };
            eprintln!("[{tag}] {name}: {preview}");
        }
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cost_usd,
            ..
        } => {
            eprintln!("[usage] in={input_tokens} out={output_tokens} ${cost_usd:.4}");
        }
        AgentEvent::BudgetWarning { fraction_used } => {
            eprintln!("[budget] {:.0}% of limit used", fraction_used * 100.0);
        }
        AgentEvent::SelfValidation {
            cmd,
            exit,
            workdir_hash,
            ..
        } => {
            eprintln!(
                "[self-validation] exit={exit} hash={} cmd={cmd}",
                &workdir_hash[..16.min(workdir_hash.len())]
            );
        }
        AgentEvent::Cancelled => eprintln!("[cancelled]"),
        AgentEvent::RunFailed { error } => eprintln!("[failed] {error}"),
        _ => {}
    }
}

#[cfg(test)]
mod serve_mcp_cli_tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;
    use truss_core::message::Block;
    use truss_core::provider::{
        CompletionRequest, CompletionResponse, ModelCapabilities, StopReason, Usage,
    };

    #[derive(Clone)]
    struct ScriptedProvider {
        calls: Arc<Mutex<usize>>,
        script: Vec<CompletionResponse>,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn default_model(&self) -> &str {
            "scripted-model"
        }

        fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(
            &self,
            _ctx: &Ctx,
            _req: &CompletionRequest,
        ) -> std::result::Result<CompletionResponse, truss_core::error::ProviderError> {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            self.script.get(index).cloned().ok_or_else(|| {
                truss_core::error::ProviderError::Config(format!(
                    "script exhausted at call {index}"
                ))
            })
        }
    }

    #[tokio::test]
    async fn cli_backend_runs_fake_contract_and_exports_signed_bundle() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "truss-serve-mcp-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let contract_path = dir.join("contract.toml");
        std::fs::write(
            &contract_path,
            r#"
[contract]
id = "serve-mcp-fake-contract"
title = "Serve MCP fake contract"
intent = "Complete a hermetic fake contract."

[budget]
max_steps = 2

[[done]]
kind = "check"
desc = "The check passes"
run = "true"
expect_exit = 0
"#,
        )
        .unwrap();
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            calls: Arc::new(Mutex::new(0)),
            script: vec![CompletionResponse {
                content: vec![Block::Text("fake contract complete".into())],
                stop_reason: StopReason::EndTurn,
                model: "scripted-model".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            }],
        });
        let db = dir.join("harness.db");
        let key = dir.join("identity.key");
        let backend = CliMcpBackend {
            config: None,
            provider: provider.clone(),
            reviewer: provider.clone(),
            memory_reviewers: vec![provider.clone()],
            tools: Arc::new(ToolRegistry::new()),
            settings: ServeMcpSettings {
                db: db.display().to_string(),
                key: key.display().to_string(),
                model: None,
                max_tokens: 1024,
                max_steps: 2,
                reasoning: None,
                temperature: None,
                max_context_msgs: 0,
                timeout: Some(10),
                memory_tool: false,
                memory_tools: false,
                spawn_tool: false,
                git_tools: false,
                github_tools: false,
                web_tools: false,
                web_search_config: truss_core::tools::WebSearchConfig::default(),
                reviewers: Some(1),
                workdir: Some(dir.display().to_string()),
                compaction_mode: None,
                compaction_threshold: None,
                compaction_min_tail: None,
                compaction_anticipate: false,
                effective_budget_ratio: None,
                effective_budget_override: Vec::new(),
            },
            jobs: Arc::new(Mutex::new(HashMap::new())),
        };
        let principal = truss_core::ctx::Principal {
            tenant: "test-tenant".into(),
            user: "test-user".into(),
        };
        let run = backend
            .run_contract(
                &principal,
                RunContractRequest {
                    contract_path: contract_path.display().to_string(),
                    session_id: None,
                    async_run: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.session_id, "serve-mcp-fake-contract");
        assert!(run.head_hash.as_deref().unwrap().len() >= 16);
        assert_eq!(
            run.bundle_ref.as_ref().unwrap().read_tool,
            truss_core::mcp_server::TOOL_LEDGER_EXPORT
        );

        let export = backend
            .ledger_export(
                &principal,
                LedgerReadRequest {
                    session_id: run.session_id,
                },
            )
            .await
            .unwrap();
        assert_eq!(export["command"], "export");
        assert_eq!(
            export["verdict"]["contract"]["id"],
            json!("serve-mcp-fake-contract")
        );
        assert!(std::fs::remove_dir_all(&dir).is_ok());
    }
}
