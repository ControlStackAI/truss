//! Model-facing delegation tool backed by the child-run substrate.
//!
//! `spawn` is intentionally store-backed: a child run must have a signed child
//! ledger and a parent `child_anchor`, so callers register this tool only for
//! durable sessions where a [`crate::store::Store`] is available.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::budget::BudgetLimits;
use crate::child_run::{run_child, ChildRunOptions, ChildRunOutput, VerifiedChildResult};
use crate::ctx::Ctx;
use crate::delegation::{ClauseResult, ClauseVerdict, Verdict};
use crate::durable::{self, DurableConfig, DurableOutcome};
use crate::error::{HarnessError, ToolError};
use crate::provider::{Provider, ToolDef};
use crate::store::Store;

use super::{Tool, ToolRegistry};

pub struct SpawnTool {
    provider: Arc<dyn Provider>,
    child_tools: Arc<ToolRegistry>,
    store: Store,
    cfg: DurableConfig,
}

impl SpawnTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        child_tools: Arc<ToolRegistry>,
        store: Store,
        cfg: DurableConfig,
    ) -> Self {
        Self {
            provider,
            child_tools,
            store,
            cfg,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SpawnInput {
    task: String,
    #[serde(default)]
    budget_slice: BudgetLimits,
    scope: Option<String>,
}

#[async_trait]
impl Tool for SpawnTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "spawn".into(),
            description: "Delegate a scoped sub-task to a child run and return its verified \
                          verdict summary, artifact refs, and ledger provenance. The child \
                          uses the shared parent budget meter plus the optional per-child \
                          budget_slice cap."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The scoped sub-task for the child run to perform."
                    },
                    "budget_slice": {
                        "type": "object",
                        "description": "Optional per-child hard cap. Omitted fields inherit only the parent tree-wide ceiling.",
                        "properties": {
                            "max_total_tokens": { "type": ["integer", "null"] },
                            "max_cost_usd": { "type": ["number", "null"] },
                            "max_steps": { "type": ["integer", "null"] }
                        }
                    },
                    "scope": {
                        "type": "string",
                        "description": "Optional human-readable scope label for provenance."
                    }
                },
                "required": ["task"]
            }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        self.run_spawn(ctx, input).await
    }

    async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        input: serde_json::Value,
        produced_by: Option<crate::memory::LedgerRef>,
    ) -> Result<String, ToolError> {
        let mut parent_ctx = ctx.clone();
        if let Some(produced_by) = produced_by {
            parent_ctx.run_id = produced_by.session;
        }
        self.run_spawn(&parent_ctx, input).await
    }
}

impl SpawnTool {
    async fn run_spawn(&self, ctx: &Ctx, input: serde_json::Value) -> Result<String, ToolError> {
        let input: SpawnInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        if input.task.trim().is_empty() {
            return Err(ToolError::InvalidInput("task must not be empty".into()));
        }

        let task = input.task;
        let scope = input.scope.unwrap_or_else(|| "spawn".into());
        let mut options = ChildRunOptions::new(task.clone(), input.budget_slice);
        options = options.with_scope(scope);

        let provider = self.provider.clone();
        let child_tools = self.child_tools.clone();
        let store = self.store.clone();
        let cfg = self.cfg.clone();

        let result = run_child(ctx, &self.store, options, move |child_ctx, child_store| {
            let provider = provider.clone();
            let child_tools = child_tools.clone();
            let cfg = cfg.clone();
            let task = task.clone();
            let store = store.clone();
            async move {
                store
                    .append(
                        &child_ctx.run_id,
                        "run_started",
                        &json!({
                            "prompt": task,
                            "model": cfg.model.clone().unwrap_or_else(|| provider.default_model().to_string()),
                            "system": cfg.system.clone(),
                        }),
                    )
                    .map_err(other)?;

                match durable::run(
                    provider,
                    child_tools,
                    &child_ctx,
                    &child_store,
                    &child_ctx.run_id,
                    &cfg,
                    None,
                )
                .await?
                {
                    DurableOutcome::Completed { final_text } => Ok(ChildRunOutput {
                        verdict: completed_verdict(&child_ctx.run_id, &task, &final_text, &child_ctx),
                        artifacts: Vec::new(),
                    }),
                    DurableOutcome::Suspended { await_desc, .. } => Err(HarnessError::Other(
                        format!("spawned child suspended awaiting {await_desc}"),
                    )),
                    DurableOutcome::StillWaiting { await_desc } => Err(HarnessError::Other(
                        format!("spawned child still waiting on {await_desc}"),
                    )),
                }
            }
        })
        .await
        .map_err(tool_exec)?;

        serde_json::to_string_pretty(&render_result(&result))
            .map_err(|e| ToolError::Execution(e.to_string()))
    }
}

fn completed_verdict(run_id: &str, task: &str, final_text: &str, ctx: &Ctx) -> Verdict {
    Verdict {
        contract_id: run_id.to_string(),
        title: "Spawned child task".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "spawn".into(),
            desc: task.to_string(),
            verdict: ClauseVerdict::Pass,
            evidence: "child durable run completed; full transcript is in the signed child ledger"
                .into(),
        }],
        executor_summary: final_text.to_string(),
        reviewed_diff: None,
        usage: ctx.budget.slice_snapshot(),
    }
}

fn render_result(result: &VerifiedChildResult) -> serde_json::Value {
    json!({
        "child_run_id": result.child_run_id,
        "verdict": {
            "overall": result.verdict.overall,
            "summary": result.verdict.executor_summary,
            "results": result.verdict.results,
        },
        "artifacts": result.artifacts,
        "ledger": {
            "verdict_ref": result.verdict_ref,
            "parent_anchor": result.parent_anchor,
            "child_root_hash": result.child_root_hash,
            "child_head_hash": result.child_head_hash,
        },
        "usage": result.usage,
    })
}

fn other(e: anyhow::Error) -> HarnessError {
    HarnessError::Other(e.to_string())
}

fn tool_exec(e: HarnessError) -> ToolError {
    match e {
        HarnessError::Cancelled => ToolError::Cancelled,
        HarnessError::Tool(e) => e,
        other => ToolError::Execution(other.to_string()),
    }
}
