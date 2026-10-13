//! Internal child-run substrate.
//!
//! A child-run executes a sub-task under its own scoped [`crate::ctx::Ctx`]:
//! parent cancellation propagates, the deadline is inherited-or-tightened, and
//! spend accrues to the parent's shared budget meter while a per-child slice is
//! enforced. The child returns a verified result ([`crate::delegation::Verdict`]
//! plus artifact refs), not a transcript. Its signed child ledger is anchored by
//! a parent `child_anchor` journal entry so normal verification checks
//! parent-to-child provenance.

use std::future::Future;
use std::time::Duration;

use serde_json::json;

use crate::budget::{BudgetLimits, BudgetSnapshot};
use crate::ctx::{Ctx, Principal};
use crate::delegation::Verdict;
use crate::error::HarnessError;
use crate::events::{AgentEvent, ArtifactRef};
use crate::memory::LedgerRef;
use crate::store::{Store, CHILD_ANCHOR_KIND};

pub const CHILD_STARTED_KIND: &str = "child_started";
pub const CHILD_RESULT_KIND: &str = "child_result";
pub const CHILD_FAILED_KIND: &str = "child_failed";

#[derive(Debug, Clone)]
pub struct ChildRunOptions {
    pub task: String,
    pub scope: String,
    pub budget_slice: BudgetLimits,
    pub timeout: Option<Duration>,
    pub principal: Option<Principal>,
}

impl ChildRunOptions {
    pub fn new(task: impl Into<String>, budget_slice: BudgetLimits) -> Self {
        Self {
            task: task.into(),
            scope: "default".into(),
            budget_slice,
            timeout: None,
            principal: None,
        }
    }

    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = Some(principal);
        self
    }
}

#[derive(Debug, Clone)]
pub struct ChildRunOutput {
    pub verdict: Verdict,
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone)]
pub struct VerifiedChildResult {
    pub child_run_id: String,
    pub verdict: Verdict,
    pub verdict_ref: LedgerRef,
    pub artifacts: Vec<ArtifactRef>,
    pub child_root_hash: String,
    pub child_head_hash: String,
    pub parent_anchor: LedgerRef,
    pub usage: BudgetSnapshot,
}

pub async fn run_child<F, Fut>(
    parent_ctx: &Ctx,
    store: &Store,
    options: ChildRunOptions,
    run: F,
) -> Result<VerifiedChildResult, HarnessError>
where
    F: FnOnce(Ctx, Store) -> Fut,
    Fut: Future<Output = Result<ChildRunOutput, HarnessError>>,
{
    parent_ctx.check()?;
    parent_ctx.budget.check()?;
    ensure_parent_session(parent_ctx, store)?;

    let child_ctx = parent_ctx.scoped_child(
        options.budget_slice.clone(),
        options.timeout,
        options.principal.clone(),
    );
    let child_run_id = child_ctx.run_id.clone();
    store
        .create_session(
            &child_run_id,
            Some("child-run"),
            &child_ctx.principal.tenant,
            &child_ctx.principal.user,
        )
        .map_err(other)?;
    store
        .append(
            &child_run_id,
            CHILD_STARTED_KIND,
            &json!({
                "parent_run_id": parent_ctx.run_id,
                "task": options.task,
                "scope": options.scope,
                "budget_slice": options.budget_slice,
            }),
        )
        .map_err(other)?;

    parent_ctx.emit(AgentEvent::ChildStarted {
        parent_run_id: parent_ctx.run_id.clone(),
        child_run_id: child_run_id.clone(),
        task: options.task.clone(),
        scope: options.scope.clone(),
        budget_slice: options.budget_slice.clone(),
    });

    let result = match run(child_ctx.clone(), store.clone()).await {
        Ok(output) => {
            child_ctx.check()?;
            child_ctx.budget.check()?;
            Ok(output)
        }
        Err(err) => Err(err),
    };

    match result {
        Ok(output) => complete_child(parent_ctx, store, &child_ctx, options, output),
        Err(err) => {
            fail_child(parent_ctx, store, &child_ctx, &options, &err)?;
            Err(err)
        }
    }
}

fn complete_child(
    parent_ctx: &Ctx,
    store: &Store,
    child_ctx: &Ctx,
    options: ChildRunOptions,
    output: ChildRunOutput,
) -> Result<VerifiedChildResult, HarnessError> {
    let verdict_entry = store
        .append(
            &child_ctx.run_id,
            CHILD_RESULT_KIND,
            &json!({
                "verdict": output.verdict,
                "artifacts": output.artifacts,
            }),
        )
        .map_err(other)?;
    store
        .set_status(&child_ctx.run_id, "completed")
        .map_err(other)?;
    let (child_root_hash, child_head_hash) = child_hashes(store, &child_ctx.run_id)?;
    let parent_anchor = anchor_child(
        parent_ctx,
        store,
        child_ctx,
        &options,
        "completed",
        &child_root_hash,
        &child_head_hash,
        Some(verdict_entry.seq),
        None,
    )?;
    let verdict_ref = LedgerRef {
        session: child_ctx.run_id.clone(),
        seq: verdict_entry.seq,
    };
    parent_ctx.emit(AgentEvent::ChildCompleted {
        parent_run_id: parent_ctx.run_id.clone(),
        child_run_id: child_ctx.run_id.clone(),
        verdict_ref: verdict_ref.clone(),
        child_head_hash: child_head_hash.clone(),
        artifacts: output.artifacts.clone(),
    });

    Ok(VerifiedChildResult {
        child_run_id: child_ctx.run_id.clone(),
        verdict: output.verdict,
        verdict_ref,
        artifacts: output.artifacts,
        child_root_hash,
        child_head_hash,
        parent_anchor,
        usage: child_ctx.budget.slice_snapshot(),
    })
}

fn fail_child(
    parent_ctx: &Ctx,
    store: &Store,
    child_ctx: &Ctx,
    options: &ChildRunOptions,
    err: &HarnessError,
) -> Result<(), HarnessError> {
    store
        .append(
            &child_ctx.run_id,
            CHILD_FAILED_KIND,
            &json!({ "error": err.to_string() }),
        )
        .map_err(other)?;
    store
        .set_status(&child_ctx.run_id, "failed")
        .map_err(other)?;
    let (child_root_hash, child_head_hash) = child_hashes(store, &child_ctx.run_id)?;
    anchor_child(
        parent_ctx,
        store,
        child_ctx,
        options,
        "failed",
        &child_root_hash,
        &child_head_hash,
        None,
        Some(&err.to_string()),
    )?;
    parent_ctx.emit(AgentEvent::ChildFailed {
        parent_run_id: parent_ctx.run_id.clone(),
        child_run_id: child_ctx.run_id.clone(),
        error: err.to_string(),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn anchor_child(
    parent_ctx: &Ctx,
    store: &Store,
    child_ctx: &Ctx,
    options: &ChildRunOptions,
    status: &str,
    child_root_hash: &str,
    child_head_hash: &str,
    verdict_seq: Option<i64>,
    error: Option<&str>,
) -> Result<LedgerRef, HarnessError> {
    let entry = store
        .append(
            &parent_ctx.run_id,
            CHILD_ANCHOR_KIND,
            &json!({
                "parent_run_id": parent_ctx.run_id,
                "child_run_id": child_ctx.run_id,
                "child_root_hash": child_root_hash,
                "child_head_hash": child_head_hash,
                "task": options.task,
                "scope": options.scope,
                "budget_slice": options.budget_slice,
                "status": status,
                "verdict_ref": verdict_seq.map(|seq| json!({
                    "session": child_ctx.run_id,
                    "seq": seq,
                })),
                "error": error,
            }),
        )
        .map_err(other)?;
    Ok(LedgerRef {
        session: parent_ctx.run_id.clone(),
        seq: entry.seq,
    })
}

fn child_hashes(store: &Store, child_run_id: &str) -> Result<(String, String), HarnessError> {
    let root = store
        .root_hash_hex(child_run_id)
        .map_err(other)?
        .ok_or_else(|| HarnessError::Other("child ledger has no root hash".into()))?;
    let head = store
        .head_hash_hex(child_run_id)
        .map_err(other)?
        .ok_or_else(|| HarnessError::Other("child ledger has no head hash".into()))?;
    Ok((root, head))
}

fn ensure_parent_session(parent_ctx: &Ctx, store: &Store) -> Result<(), HarnessError> {
    if store
        .get_session(&parent_ctx.run_id)
        .map_err(other)?
        .is_none()
    {
        store
            .create_session(
                &parent_ctx.run_id,
                Some("child-run-parent"),
                &parent_ctx.principal.tenant,
                &parent_ctx.principal.user,
            )
            .map_err(other)?;
    }
    Ok(())
}

fn other(e: anyhow::Error) -> HarnessError {
    HarnessError::Other(e.to_string())
}
