//! The run context — the single handle threaded through every provider and tool
//! call.
//!
//! `Ctx` is the architectural keystone. It carries, in one cheap-to-clone value:
//! cancellation, a deadline, the shared budget meter, the calling principal
//! (tenant/user), an event sink, and the enforced policy. Because *one* handle
//! carries all of it, adding a new cross-cutting concern later (rate-limit keys,
//! distributed trace spans, quotas) never changes the `complete`/`call`
//! signatures again — it adds a field here.
//!
//! Sub-agents get a [`Ctx::child`]: a child cancellation token (cancelling the
//! parent cancels children, but not vice-versa) sharing the same budget pool.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::budget::{BudgetLimits, BudgetMeter};
use crate::error::HarnessError;
use crate::events::AgentEvent;
use crate::policy::Policy;
use crate::sandbox::{NoSandbox, Sandbox};

/// Who is making this request. Drives multi-tenant isolation and audit later.
#[derive(Debug, Clone)]
pub struct Principal {
    pub tenant: String,
    pub user: String,
}

impl Default for Principal {
    fn default() -> Self {
        Principal {
            tenant: "local".into(),
            user: "local".into(),
        }
    }
}

type EventSink = tokio::sync::mpsc::UnboundedSender<AgentEvent>;

#[derive(Clone)]
pub struct Ctx {
    pub run_id: String,
    pub principal: Principal,
    pub policy: Arc<Policy>,
    pub sandbox: Arc<dyn Sandbox>,
    pub budget: BudgetMeter,
    cancel: CancellationToken,
    deadline: Option<Instant>,
    events: Option<EventSink>,
}

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A monotonic-ish unique id without pulling in a uuid dependency.
fn gen_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("run-{nanos:x}-{n:x}")
}

impl Ctx {
    /// A fresh root context with default principal/policy and no limits.
    pub fn new() -> Self {
        Ctx {
            run_id: gen_run_id(),
            principal: Principal::default(),
            policy: Arc::new(Policy::default()),
            sandbox: Arc::new(NoSandbox),
            budget: BudgetMeter::default(),
            cancel: CancellationToken::new(),
            deadline: None,
            events: None,
        }
    }

    // ---- builder-style setters -------------------------------------------

    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = principal;
        self
    }

    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_budget(mut self, budget: BudgetMeter) -> Self {
        self.budget = budget;
        self
    }

    /// Wall-clock deadline measured from now.
    pub fn with_timeout(mut self, dur: Duration) -> Self {
        self.deadline = Some(Instant::now() + dur);
        self
    }

    pub fn with_events(mut self, sink: EventSink) -> Self {
        self.events = Some(sink);
        self
    }

    // ---- cancellation & deadlines ----------------------------------------

    /// A child context: cancelling the parent cancels the child, sharing the same
    /// budget pool, principal, policy, and event sink.
    pub fn child(&self) -> Ctx {
        Ctx {
            run_id: gen_run_id(),
            principal: self.principal.clone(),
            policy: self.policy.clone(),
            sandbox: self.sandbox.clone(),
            budget: self.budget.clone(),
            cancel: self.cancel.child_token(),
            deadline: self.deadline,
            events: self.events.clone(),
        }
    }

    /// A child context with a per-child budget slice and an inherited-or-tighter
    /// deadline. Spend still accrues to the parent's shared tree-wide meter.
    pub fn scoped_child(
        &self,
        budget_slice: BudgetLimits,
        timeout: Option<Duration>,
        principal: Option<Principal>,
    ) -> Ctx {
        let requested_deadline = timeout.map(|dur| Instant::now() + dur);
        let deadline = match (self.deadline, requested_deadline) {
            (Some(parent), Some(child)) => Some(parent.min(child)),
            (Some(parent), None) => Some(parent),
            (None, Some(child)) => Some(child),
            (None, None) => None,
        };
        Ctx {
            run_id: gen_run_id(),
            principal: principal.unwrap_or_else(|| self.principal.clone()),
            policy: self.policy.clone(),
            sandbox: self.sandbox.clone(),
            budget: self.budget.child_slice(budget_slice),
            cancel: self.cancel.child_token(),
            deadline,
            events: self.events.clone(),
        }
    }

    /// Same-run cancellation scope for orchestration layers that need to cancel
    /// a group of descendant child contexts without cancelling the caller. The
    /// run id is intentionally preserved so child-run anchors still land in the
    /// parent ledger.
    pub(crate) fn same_run_cancellation_scope(&self) -> Ctx {
        Ctx {
            run_id: self.run_id.clone(),
            principal: self.principal.clone(),
            policy: self.policy.clone(),
            sandbox: self.sandbox.clone(),
            budget: self.budget.clone(),
            cancel: self.cancel.child_token(),
            deadline: self.deadline,
            events: self.events.clone(),
        }
    }

    /// Trigger cancellation (e.g. from a Ctrl-C handler).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Clone of the raw token, for `tokio::select!` inside provider/tool calls.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Time left before the deadline, if one is set and not yet passed.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Cheap guard: call at loop/tool boundaries. Errors if cancelled or expired.
    pub fn check(&self) -> Result<(), HarnessError> {
        if self.cancel.is_cancelled() {
            return Err(HarnessError::Cancelled);
        }
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                return Err(HarnessError::Cancelled);
            }
        }
        Ok(())
    }

    // ---- events -----------------------------------------------------------

    pub fn emit(&self, event: AgentEvent) {
        if let Some(sink) = &self.events {
            // A closed receiver just means nobody's listening; not an error.
            let _ = sink.send(event);
        }
    }
}

impl Default for Ctx {
    fn default() -> Self {
        Self::new()
    }
}
