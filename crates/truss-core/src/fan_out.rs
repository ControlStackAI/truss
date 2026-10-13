//! Best-of-N fan-out on the child-run substrate.
//!
//! Fan-out is deliberately thin: each branch is a normal
//! [`crate::child_run::run_child`] with its own scoped `Ctx`, budget slice, child
//! ledger, and parent `child_anchor`. This module only launches sibling
//! child-runs, reduces their verified results, and records one signed
//! `fanout_reduction` decision in the parent ledger.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{
    future::join_all,
    stream::{FuturesUnordered, StreamExt},
};
use serde_json::json;

use crate::budget::BudgetLimits;
use crate::child_run::{run_child, ChildRunOptions, ChildRunOutput, VerifiedChildResult};
use crate::contract::{BudgetSpec, ConstraintSpec, Contract, DoneClause, ExecutorSpec, Meta};
use crate::ctx::{Ctx, Principal};
use crate::delegation::{ClauseResult, ClauseVerdict, ReviewedDiff};
use crate::error::HarnessError;
use crate::events::{AgentEvent, FanOutChildRef, FanOutReducerKind, FanOutWinner};
use crate::memory::LedgerRef;
use crate::message::Message;
use crate::provider::CompletionRequest;
use crate::provider::Provider;
use crate::store::{Store, CHILD_ANCHOR_KIND, FANOUT_REDUCTION_KIND};

static FANOUT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Caller-supplied scorer for [`Reducer::BestScore`].
pub trait BestScore: Send + Sync {
    fn score(&self, result: &VerifiedChildResult) -> f64;

    fn explain(&self, result: &VerifiedChildResult, score: f64) -> String {
        format!("{} scored {score:.4}", result.child_run_id)
    }
}

/// Selects or merges the verified child results.
#[derive(Clone, Default)]
pub enum Reducer {
    /// Strict majority over verified-result `overall` verdicts.
    #[default]
    Vote,
    /// Score each child by reusing the existing delegation reviewer panel.
    JudgePanel {
        reviewers: Vec<Arc<dyn Provider>>,
        criterion: String,
    },
    /// Pick the highest caller-supplied score.
    BestScore { scorer: Arc<dyn BestScore> },
    /// Pick the first child whose verdict passes and cancel sibling branches.
    FirstSuccess,
}

impl Reducer {
    pub fn kind(&self) -> FanOutReducerKind {
        match self {
            Reducer::Vote => FanOutReducerKind::Vote,
            Reducer::JudgePanel { .. } => FanOutReducerKind::JudgePanel,
            Reducer::BestScore { .. } => FanOutReducerKind::BestScore,
            Reducer::FirstSuccess => FanOutReducerKind::FirstSuccess,
        }
    }
}

/// Serializable reducer knob for config surfaces that bind concrete runtime
/// dependencies separately.
pub type ReducerConfig = FanOutReducerKind;

pub struct FanOutOptions {
    pub task: String,
    pub scope: String,
    pub k: usize,
    pub budget_slice: BudgetLimits,
    pub timeout: Option<Duration>,
    pub principal: Option<Principal>,
    pub reducer: Reducer,
}

impl FanOutOptions {
    pub fn new(task: impl Into<String>, k: usize, budget_slice: BudgetLimits) -> Self {
        Self {
            task: task.into(),
            scope: "default".into(),
            k,
            budget_slice,
            timeout: None,
            principal: None,
            reducer: Reducer::default(),
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

    pub fn with_reducer(mut self, reducer: Reducer) -> Self {
        self.reducer = reducer;
        self
    }
}

#[derive(Debug, Clone)]
pub struct FanOutBranch {
    pub fanout_id: String,
    pub index: usize,
    pub task: String,
}

#[derive(Debug, Clone)]
pub struct FanOutResult {
    pub fanout_id: String,
    pub reducer: FanOutReducerKind,
    pub child_results: Vec<FanOutChildRef>,
    pub winner: FanOutWinner,
    pub why: String,
    pub decision_ref: LedgerRef,
}

struct BranchOutcome {
    index: usize,
    result: Result<VerifiedChildResult, HarnessError>,
}

struct BranchVerified {
    index: usize,
    result: VerifiedChildResult,
}

struct Reduction {
    index: usize,
    why: String,
}

/// Run `k` child-runs of the same task concurrently and reduce the verified
/// results.
pub async fn run_fan_out<F, Fut>(
    parent_ctx: &Ctx,
    store: &Store,
    options: FanOutOptions,
    run: F,
) -> Result<FanOutResult, HarnessError>
where
    F: Fn(FanOutBranch, Ctx, Store) -> Fut + Clone,
    Fut: Future<Output = Result<ChildRunOutput, HarnessError>>,
{
    parent_ctx.check()?;
    parent_ctx.budget.check()?;
    if options.k == 0 {
        return Err(HarnessError::Other(
            "fan-out requires at least one branch".into(),
        ));
    }
    ensure_parent_session(parent_ctx, store)?;

    let fanout_id = gen_fanout_id();
    let reducer_kind = options.reducer.kind();
    parent_ctx.emit(AgentEvent::FanoutStarted {
        parent_run_id: parent_ctx.run_id.clone(),
        fanout_id: fanout_id.clone(),
        task: options.task.clone(),
        k: options.k,
        reducer: reducer_kind.clone(),
    });

    let fanout_ctx = parent_ctx.same_run_cancellation_scope();
    let mut tasks = FuturesUnordered::new();
    for index in 0..options.k {
        let runner = run.clone();
        let parent = fanout_ctx.clone();
        let store = store.clone();
        let branch = FanOutBranch {
            fanout_id: fanout_id.clone(),
            index,
            task: options.task.clone(),
        };
        let child_options = ChildRunOptions {
            task: options.task.clone(),
            scope: branch_scope(&fanout_id, index, &options.scope),
            budget_slice: options.budget_slice.clone(),
            timeout: options.timeout,
            principal: options.principal.clone(),
        };
        tasks.push(async move {
            let result = run_child(&parent, &store, child_options, move |ctx, child_store| {
                runner(branch.clone(), ctx, child_store)
            })
            .await;
            BranchOutcome { index, result }
        });
    }

    let mut outcomes = Vec::with_capacity(options.k);
    while let Some(outcome) = tasks.next().await {
        if matches!(options.reducer, Reducer::FirstSuccess)
            && outcome
                .result
                .as_ref()
                .is_ok_and(|result| result.verdict.overall == ClauseVerdict::Pass)
        {
            fanout_ctx.cancel();
        }
        outcomes.push(outcome);
    }

    let verified = outcomes
        .into_iter()
        .filter_map(|outcome| {
            outcome.result.ok().map(|result| BranchVerified {
                index: outcome.index,
                result,
            })
        })
        .collect::<Vec<_>>();

    if verified.is_empty() {
        if parent_ctx.check().is_err() {
            return Err(HarnessError::Cancelled);
        }
        parent_ctx.budget.check()?;
        return Err(HarnessError::Other(
            "fan-out produced no verified child results".into(),
        ));
    }

    let reduction = reduce(&options.reducer, &fanout_id, parent_ctx, &verified).await?;
    let child_results = collect_child_refs(store, &parent_ctx.run_id, &fanout_id, options.k)?;
    let winning_ref = child_results
        .iter()
        .find(|child| child.index == reduction.index && child.status == "completed")
        .ok_or_else(|| {
            HarnessError::Other("fan-out winner has no completed child anchor".into())
        })?;
    let verdict_ref = winning_ref
        .verdict_ref
        .clone()
        .ok_or_else(|| HarnessError::Other("fan-out winner has no verdict ref".into()))?;
    let winner = FanOutWinner {
        index: winning_ref.index,
        child_run_id: winning_ref.child_run_id.clone(),
        verdict_ref,
    };
    let decision_ref = record_reduction(
        parent_ctx,
        store,
        &fanout_id,
        reducer_kind.clone(),
        &child_results,
        &winner,
        &reduction.why,
    )?;

    parent_ctx.emit(AgentEvent::FanoutCompleted {
        parent_run_id: parent_ctx.run_id.clone(),
        fanout_id: fanout_id.clone(),
        reducer: reducer_kind.clone(),
        child_results: child_results.clone(),
        winner: Some(winner.clone()),
        why: reduction.why.clone(),
        decision_ref: decision_ref.clone(),
    });

    Ok(FanOutResult {
        fanout_id,
        reducer: reducer_kind,
        child_results,
        winner,
        why: reduction.why,
        decision_ref,
    })
}

async fn reduce(
    reducer: &Reducer,
    fanout_id: &str,
    ctx: &Ctx,
    verified: &[BranchVerified],
) -> Result<Reduction, HarnessError> {
    match reducer {
        Reducer::Vote => reduce_vote(verified),
        Reducer::JudgePanel {
            reviewers,
            criterion,
        } => reduce_judge_panel(fanout_id, ctx, reviewers, criterion, verified).await,
        Reducer::BestScore { scorer } => reduce_best_score(scorer.as_ref(), verified),
        Reducer::FirstSuccess => verified
            .iter()
            .find(|child| child.result.verdict.overall == ClauseVerdict::Pass)
            .map(|child| Reduction {
                index: child.index,
                why: format!(
                    "first successful child {} passed verification",
                    child.result.child_run_id
                ),
            })
            .ok_or_else(|| HarnessError::Other("first-success reducer found no pass".into())),
    }
}

fn reduce_vote(verified: &[BranchVerified]) -> Result<Reduction, HarnessError> {
    let mut counts: Vec<(ClauseVerdict, usize)> = Vec::new();
    for child in verified {
        if let Some((_, count)) = counts
            .iter_mut()
            .find(|(verdict, _)| *verdict == child.result.verdict.overall)
        {
            *count += 1;
        } else {
            counts.push((child.result.verdict.overall.clone(), 1));
        }
    }
    let Some((winner_verdict, count)) = counts
        .into_iter()
        .find(|(_, count)| *count * 2 > verified.len())
    else {
        return Err(HarnessError::Other(
            "vote reducer found no strict majority".into(),
        ));
    };
    let winner = verified
        .iter()
        .find(|child| child.result.verdict.overall == winner_verdict)
        .expect("majority verdict has at least one child");
    Ok(Reduction {
        index: winner.index,
        why: format!(
            "majority {:?} verdict ({count}/{})",
            winner_verdict,
            verified.len()
        ),
    })
}

async fn reduce_judge_panel(
    fanout_id: &str,
    ctx: &Ctx,
    reviewers: &[Arc<dyn Provider>],
    criterion: &str,
    verified: &[BranchVerified],
) -> Result<Reduction, HarnessError> {
    let contract = Contract {
        contract: Meta {
            id: format!("fanout-{fanout_id}"),
            title: "Fan-out reduction".into(),
            intent: "Select the best verified child result from a fan-out.".into(),
        },
        done: Vec::new(),
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec::default(),
        reviewers: Some(reviewers.len().max(1)),
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: ExecutorSpec::default(),
    };
    let clause = DoneClause {
        kind: "review".into(),
        desc: criterion.to_string(),
        run: None,
        expect_exit: None,
        expect_contains: Vec::new(),
        reviewer_context: Vec::new(),
    };

    let mut best: Option<(usize, usize, ClauseVerdict, String, String)> = None;
    for child in verified {
        let summary = child_summary(child);
        let panel = run_judge_panel(reviewers, ctx, &contract, &clause, &summary, None).await;
        let met_count = parse_panel_met_count(&panel.evidence).unwrap_or(0);
        if best
            .as_ref()
            .is_none_or(
                |(_, best_count, best_verdict, _, _)| match met_count.cmp(best_count) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Equal => {
                        verdict_rank(&panel.verdict) > verdict_rank(best_verdict)
                    }
                    std::cmp::Ordering::Less => false,
                },
            )
        {
            best = Some((
                child.index,
                met_count,
                panel.verdict,
                panel.evidence,
                child.result.child_run_id.clone(),
            ));
        }
    }
    best.map(|(index, met_count, verdict, evidence, child_run_id)| Reduction {
        index,
        why: format!(
            "judge panel selected child {child_run_id} with {met_count} met votes ({verdict:?}): {evidence}"
        ),
    })
    .ok_or_else(|| HarnessError::Other("judge-panel reducer had no verified children".into()))
}

/// Run a no-stake reviewer panel and return the same clause-level result used
/// by verified delegation. Delegation review clauses and fan-out JudgePanel
/// reduction both route through this helper so reviewer semantics stay aligned.
pub async fn run_judge_panel(
    reviewers: &[Arc<dyn Provider>],
    ctx: &Ctx,
    contract: &Contract,
    clause: &DoneClause,
    work_summary: &str,
    reviewed_diff: Option<&ReviewedDiff>,
) -> ClauseResult {
    let mut context = String::new();
    for path in &clause.reviewer_context {
        if let Ok(text) = std::fs::read_to_string(path) {
            context.push_str(&format!("\n--- {path} ---\n{}\n", truncate(&text, 2000)));
        }
    }

    let n = reviewers.len().max(1);
    let votes = join_all(reviewers.iter().enumerate().map(|(i, reviewer)| {
        let lens = REVIEW_LENSES[i % REVIEW_LENSES.len()];
        one_review(
            reviewer.clone(),
            ctx,
            contract,
            clause,
            work_summary,
            reviewed_diff,
            &context,
            lens,
            i + 1,
        )
    }))
    .await;

    let met_count = votes.iter().filter(|v| v.met == Some(true)).count();
    let verdict = if met_count * 2 > n {
        ClauseVerdict::Pass
    } else if votes.iter().all(|v| v.met.is_none()) {
        ClauseVerdict::Unverifiable
    } else {
        ClauseVerdict::Fail
    };

    let mut evidence = format!("panel: {met_count}/{n} reviewers say met\n");
    for v in &votes {
        evidence.push_str(&format!("  {}\n", v.line));
    }

    ClauseResult {
        kind: "review".into(),
        desc: clause.desc.clone(),
        verdict,
        evidence: evidence.trim_end().to_string(),
    }
}

const REVIEW_LENSES: &[&str] = &[
    "whether the work actually satisfies the stated intent, not just resembles it",
    "completeness — anything the criterion requires that is missing",
    "correctness — subtle errors, wrong assumptions, or off-by-one mistakes",
    "shortcuts — any place the work cut a corner, stubbed, or faked progress",
];

struct PanelVote {
    met: Option<bool>,
    line: String,
}

#[allow(clippy::too_many_arguments)]
async fn one_review(
    reviewer: Arc<dyn Provider>,
    ctx: &Ctx,
    contract: &Contract,
    clause: &DoneClause,
    work_summary: &str,
    reviewed_diff: Option<&ReviewedDiff>,
    context: &str,
    lens: &str,
    seat: usize,
) -> PanelVote {
    let system = format!(
        "You are an adversarial, no-stake reviewer. You did NOT do the work and gain \
         nothing from approving it. Your job is to find a concrete reason the work does NOT \
         satisfy the criterion. Compare the executor's claimed summary against the actual \
         changes and flag any unsupported or contradictory claim. Scrutinize especially: {lens}. Approve only if you cannot \
         find a real problem. Respond with ONLY a JSON object: \
         {{\"met\": true|false, \"reasoning\": \"...\"}}."
    );
    let diff_section = match reviewed_diff {
        Some(diff) => format!(
            "ACTUAL CHANGES (diff):\nSource: {}\nFull diff sha256: {}\nReviewed diff sha256: {}\nBytes reviewed: {} of {}{}\n\n{}\n",
            diff.source,
            diff.full_sha256,
            diff.reviewed_sha256,
            diff.reviewed_bytes,
            diff.full_bytes,
            if diff.truncated { " (truncated)" } else { "" },
            if diff.content.is_empty() {
                "[no git working-tree changes captured]"
            } else {
                &diff.content
            }
        ),
        None => "ACTUAL CHANGES (diff):\n[not supplied for this review]\n".to_string(),
    };
    let prompt = format!(
        "Intent:\n{}\n\nCriterion to judge:\n{}\n\nEXECUTOR'S CLAIMED SUMMARY:\n{}\n\n{}\nAdditional reviewer context:\n{}",
        contract.contract.intent, clause.desc, work_summary, diff_section, context
    );

    let mut req = CompletionRequest::new(vec![Message::user(prompt)]);
    req.system = Some(system);
    req.max_tokens = 512;

    match reviewer.complete(ctx, &req).await {
        Ok(resp) => {
            let text = Message {
                role: crate::message::Role::Assistant,
                content: resp.content,
            }
            .text();
            match parse_review(&text) {
                Some((met, reasoning)) => PanelVote {
                    met: Some(met),
                    line: format!(
                        "[{seat}/{}] {} — {reasoning}",
                        reviewer.name(),
                        if met { "met" } else { "REFUTED" }
                    ),
                },
                None => PanelVote {
                    met: None,
                    line: format!(
                        "[{seat}/{}] unparseable: {}",
                        reviewer.name(),
                        truncate(&text, 120)
                    ),
                },
            }
        }
        Err(e) => PanelVote {
            met: None,
            line: format!("[{seat}/{}] error: {e}", reviewer.name()),
        },
    }
}

fn parse_review(text: &str) -> Option<(bool, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(text.get(start..=end)?).ok()?;
    let met = v["met"].as_bool()?;
    let reasoning = v["reasoning"].as_str().unwrap_or("").to_string();
    Some((met, reasoning))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn verdict_rank(verdict: &ClauseVerdict) -> u8 {
    match verdict {
        ClauseVerdict::Pass => 2,
        ClauseVerdict::Fail => 1,
        ClauseVerdict::Unverifiable => 0,
    }
}

fn parse_panel_met_count(evidence: &str) -> Option<usize> {
    let first = evidence.lines().next()?;
    let rest = first.strip_prefix("panel: ")?;
    let (count, _) = rest.split_once('/')?;
    count.parse().ok()
}

fn reduce_best_score(
    scorer: &dyn BestScore,
    verified: &[BranchVerified],
) -> Result<Reduction, HarnessError> {
    let mut best: Option<(&BranchVerified, f64)> = None;
    for child in verified {
        let score = scorer.score(&child.result);
        if !score.is_finite() {
            return Err(HarnessError::Other(format!(
                "best-score reducer produced non-finite score for {}",
                child.result.child_run_id
            )));
        }
        if best
            .as_ref()
            .is_none_or(|(_, best_score)| score > *best_score)
        {
            best = Some((child, score));
        }
    }
    let (child, score) =
        best.ok_or_else(|| HarnessError::Other("best-score reducer had no children".into()))?;
    Ok(Reduction {
        index: child.index,
        why: scorer.explain(&child.result, score),
    })
}

fn child_summary(child: &BranchVerified) -> String {
    let verdict = &child.result.verdict;
    let mut summary = format!(
        "Child index: {}\nChild run: {}\nOverall: {:?}\nExecutor summary:\n{}\n",
        child.index, child.result.child_run_id, verdict.overall, verdict.executor_summary
    );
    for clause in &verdict.results {
        summary.push_str(&format!(
            "\n- {:?} {}: {}\n  evidence: {}",
            clause.verdict, clause.kind, clause.desc, clause.evidence
        ));
    }
    if !child.result.artifacts.is_empty() {
        summary.push_str("\nArtifacts:\n");
        for artifact in &child.result.artifacts {
            summary.push_str(&format!(
                "- {} {} {} bytes sha256={}\n",
                artifact.kind, artifact.path, artifact.size_bytes, artifact.sha256
            ));
        }
    }
    summary
}

fn record_reduction(
    parent_ctx: &Ctx,
    store: &Store,
    fanout_id: &str,
    reducer: FanOutReducerKind,
    children: &[FanOutChildRef],
    winner: &FanOutWinner,
    why: &str,
) -> Result<LedgerRef, HarnessError> {
    let entry = store
        .append(
            &parent_ctx.run_id,
            FANOUT_REDUCTION_KIND,
            &json!({
                "parent_run_id": parent_ctx.run_id,
                "fanout_id": fanout_id,
                "reducer": reducer,
                "children": children,
                "winner": winner,
                "why": why,
            }),
        )
        .map_err(other)?;
    Ok(LedgerRef {
        session: parent_ctx.run_id.clone(),
        seq: entry.seq,
    })
}

fn collect_child_refs(
    store: &Store,
    parent_run_id: &str,
    fanout_id: &str,
    expected: usize,
) -> Result<Vec<FanOutChildRef>, HarnessError> {
    let mut refs = Vec::new();
    for entry in store.entries(parent_run_id).map_err(other)? {
        if entry.kind != CHILD_ANCHOR_KIND {
            continue;
        }
        let Some(scope) = entry.payload.get("scope").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(index) = parse_branch_index(fanout_id, scope) else {
            continue;
        };
        let child_run_id = string_payload(&entry.payload, "child_run_id")?;
        let status = string_payload(&entry.payload, "status")?;
        let child_head_hash = string_payload(&entry.payload, "child_head_hash")?;
        refs.push(FanOutChildRef {
            index,
            child_run_id,
            status,
            parent_anchor: LedgerRef {
                session: parent_run_id.to_string(),
                seq: entry.seq,
            },
            verdict_ref: ledger_ref_from_value(entry.payload.get("verdict_ref"))?,
            child_head_hash,
        });
    }
    refs.sort_by_key(|child| child.index);
    if refs.len() != expected {
        return Err(HarnessError::Other(format!(
            "fan-out expected {expected} child anchors, found {}",
            refs.len()
        )));
    }
    Ok(refs)
}

fn ledger_ref_from_value(
    value: Option<&serde_json::Value>,
) -> Result<Option<LedgerRef>, HarnessError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let session = value
        .get("session")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HarnessError::Other("ledger ref missing session".into()))?;
    let seq = value
        .get("seq")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| HarnessError::Other("ledger ref missing seq".into()))?;
    Ok(Some(LedgerRef {
        session: session.to_string(),
        seq,
    }))
}

fn string_payload(payload: &serde_json::Value, key: &str) -> Result<String, HarnessError> {
    payload
        .get(key)
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| HarnessError::Other(format!("child anchor missing {key}")))
}

fn branch_scope(fanout_id: &str, index: usize, scope: &str) -> String {
    format!("fanout:{fanout_id}:branch:{index}:{scope}")
}

fn parse_branch_index(fanout_id: &str, scope: &str) -> Option<usize> {
    let rest = scope.strip_prefix(&format!("fanout:{fanout_id}:branch:"))?;
    rest.split(':').next()?.parse().ok()
}

fn gen_fanout_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = FANOUT_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("fanout-{nanos:x}-{n:x}")
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
                Some("fan-out-parent"),
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
