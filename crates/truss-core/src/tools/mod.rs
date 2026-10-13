//! Tools the model can call, plus a registry to dispatch them.
//!
//! A [`Tool`] is the mirror image of a provider call: the model emits a
//! `ToolUse` block, the harness runs the matching tool, and feeds the result
//! back. Every tool receives the [`Ctx`] so it can honor cancellation and
//! enforce [`crate::policy::Policy`] — tools are where the agent touches the real
//! world, so this is the boundary that has to hold.

mod bash;
mod fs;
mod git;
mod github;
mod identity;
pub mod memory;
mod provider_alias;
mod self_check;
mod spawn;
mod web;

pub use bash::Bash;
pub use fs::{ListDir, ReadFile, WriteFile};
pub use git::{GitJournal, GitTool};
pub use github::{GitHubJournal, GitHubTool};
pub use identity::{AmendIdentityTool, BufferedIdentityAmendment};
pub use memory::{MemoryReadJournal, MemoryReadTool, MemoryTool};
pub use self_check::SelfCheckTool;
pub use spawn::SpawnTool;
pub use web::{
    WebFetchResponse, WebHttpTransport, WebJournal, WebSearchConfig, WebSearchProvider,
    WebSearchResult, WebTool,
};

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::memory::{LedgerRef, MemoryConfig};
use crate::provider::{Provider, ToolDef};

#[async_trait]
pub trait Tool: Send + Sync {
    /// The schema advertised to the model.
    fn def(&self) -> ToolDef;
    /// Execute the tool with the model-provided input.
    async fn call(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError>;
    /// Execute the tool with an optional durable-journal provenance pointer.
    async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        input: serde_json::Value,
        _produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        self.call(ctx, input).await
    }
}

/// Holds the registered tools and dispatches calls by name, enforcing policy.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the standard local toolset (read/write/list/bash).
    pub fn with_defaults() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(ReadFile));
        r.register(Arc::new(WriteFile));
        r.register(Arc::new(ListDir));
        r.register(Arc::new(Bash));
        r
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.def().name, tool);
    }

    /// Register the governed memory tool. Callers decide whether to advertise it
    /// for a session; default registries deliberately do not include it.
    pub fn register_memory(&mut self, store: Arc<dyn crate::memory::MemoryStore>) {
        self.register(Arc::new(MemoryTool::new(store)));
    }

    /// Register governed memory with a resolved verification policy and the
    /// reviewer panel used for promotion.
    pub fn register_memory_with_promotion(
        &mut self,
        store: Arc<dyn crate::memory::MemoryStore>,
        config: MemoryConfig,
        reviewers: Vec<Arc<dyn Provider>>,
    ) {
        self.register(Arc::new(MemoryTool::with_promotion(
            store, config, reviewers,
        )));
    }

    /// Register governed memory with promotion verdicts also recorded into the
    /// signed durable journal.
    pub fn register_memory_with_signed_promotion(
        &mut self,
        store: Arc<dyn crate::memory::MemoryStore>,
        config: MemoryConfig,
        reviewers: Vec<Arc<dyn Provider>>,
        journal: crate::store::Store,
    ) {
        self.register(Arc::new(MemoryTool::with_signed_promotion(
            store, config, reviewers, journal,
        )));
    }

    /// Register read-only model-pulled durable memory tools. These are separate
    /// from the governed `memory` write binding so recall can be enabled without
    /// giving the model a new write/promote path.
    pub fn register_memory_read_tools(
        &mut self,
        store: Arc<dyn crate::memory::MemoryStore>,
        journal: Option<MemoryReadJournal>,
        recall: crate::memory::MemoryRecallMode,
    ) {
        self.register(Arc::new(MemoryReadTool::query(
            store.clone(),
            journal.clone(),
            recall,
        )));
        self.register(Arc::new(MemoryReadTool::view(store, journal, recall)));
    }

    /// Register the store-backed model-facing spawn tool. Callers bind the
    /// provider, child tool registry, durable config, and store explicitly so
    /// default registries remain side-effect free and spawn stays opt-in.
    pub fn register_spawn(
        &mut self,
        provider: Arc<dyn Provider>,
        child_tools: Arc<ToolRegistry>,
        store: crate::store::Store,
        cfg: crate::durable::DurableConfig,
    ) {
        self.register(Arc::new(SpawnTool::new(provider, child_tools, store, cfg)));
    }

    pub fn register_self_check(
        &mut self,
        store: crate::store::Store,
        session_id: impl Into<String>,
        declared_command: Option<String>,
    ) {
        let session_id = session_id.into();
        self.register(Arc::new(SelfCheckTool::new(
            "self_check",
            store.clone(),
            session_id.clone(),
            declared_command.clone(),
        )));
        self.register(Arc::new(SelfCheckTool::new(
            "run_tests",
            store,
            session_id,
            declared_command,
        )));
    }

    pub fn register_git_tools(&mut self, journal: Option<GitJournal>) {
        self.register(Arc::new(GitTool::status(journal.clone())));
        self.register(Arc::new(GitTool::diff(journal.clone())));
        self.register(Arc::new(GitTool::branch(journal.clone())));
        self.register(Arc::new(GitTool::commit(journal)));
    }

    pub fn register_github_tools(&mut self, journal: Option<GitHubJournal>) {
        self.register(Arc::new(GitHubTool::pr_view(journal.clone())));
        self.register(Arc::new(GitHubTool::pr_create(journal.clone())));
        self.register(Arc::new(GitHubTool::pr_diff(journal.clone())));
        self.register(Arc::new(GitHubTool::pr_checks(journal.clone())));
        self.register(Arc::new(GitHubTool::issue_view(journal.clone())));
        self.register(Arc::new(GitHubTool::issue_list(journal.clone())));
        self.register(Arc::new(GitHubTool::check_status(journal)));
    }

    pub fn register_web_tools(
        &mut self,
        journal: Option<WebJournal>,
        search_config: WebSearchConfig,
    ) {
        self.register(Arc::new(WebTool::fetch(
            journal.clone(),
            search_config.clone(),
        )));
        self.register(Arc::new(WebTool::search(journal, search_config)));
    }

    pub fn register_amend_identity(
        &mut self,
        persona: crate::memory::Persona,
        policy: crate::memory::IdentityPolicy,
        buffer: Arc<std::sync::Mutex<Vec<BufferedIdentityAmendment>>>,
    ) {
        self.register(Arc::new(AmendIdentityTool::new(persona, policy, buffer)));
    }

    /// Discover and register tools from configured MCP servers.
    pub async fn register_mcp_servers(
        &mut self,
        ctx: &Ctx,
        servers: &[crate::mcp::McpServerConfig],
        journal: Option<crate::mcp::McpJournal>,
    ) -> Result<(), ToolError> {
        crate::mcp::mount_mcp_servers(self, ctx, servers, journal).await
    }

    /// All registered tool definitions.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.def()).collect()
    }

    /// Tool definitions the given context's policy permits advertising.
    pub fn defs_for(&self, ctx: &Ctx) -> Vec<ToolDef> {
        self.tools
            .values()
            .filter(|t| ctx.policy.can_use_tool(&t.def().name))
            .map(|t| t.def())
            .collect()
    }

    pub async fn call(
        &self,
        ctx: &Ctx,
        name: &str,
        input: serde_json::Value,
    ) -> Result<String, ToolError> {
        if let Some(result) =
            provider_alias::call_provider_alias(self, ctx, name, input.clone()).await
        {
            return result;
        }
        self.call_registered(ctx, name, input).await
    }

    pub(super) async fn call_registered(
        &self,
        ctx: &Ctx,
        name: &str,
        input: serde_json::Value,
    ) -> Result<String, ToolError> {
        if !ctx.policy.can_use_tool(name) {
            return Err(ToolError::PermissionDenied(format!(
                "tool '{name}' not allowed"
            )));
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?
            .clone();
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        tool.call(ctx, input).await
    }

    pub async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        name: &str,
        input: serde_json::Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        if let Some(result) =
            provider_alias::call_provider_alias(self, ctx, name, input.clone()).await
        {
            return result;
        }
        if !ctx.policy.can_use_tool(name) {
            return Err(ToolError::PermissionDenied(format!(
                "tool '{name}' not allowed"
            )));
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::NotFound(name.to_string()))?
            .clone();
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        tool.call_with_ledger(ctx, input, produced_by).await
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
