//! Paired learning-on/off evaluation harness (SI-4).
//!
//! This is measurement only: it runs the same contract in paired child-run arms,
//! flips only the memory policy knobs, and records a structured report. It does
//! not tune, apply, or persist any policy change from the result.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::budget::{BudgetLimits, BudgetSnapshot};
use crate::child_run::{run_child, ChildRunOptions, ChildRunOutput, VerifiedChildResult};
use crate::contract::Contract;
use crate::ctx::Ctx;
use crate::delegation::{self, ClauseVerdict, ContractCloseLearning};
use crate::durable::DurableConfig;
use crate::error::HarnessError;
use crate::events::ArtifactRef;
use crate::executor::Executor;
use crate::memory::{
    GoalLedger, LedgerRef, LessonExtractionPolicy, MemoryConfig, MemoryStore, VerificationPolicy,
};
use crate::provider::Provider;
use crate::store::Store;
use crate::tools::ToolRegistry;

pub const EVAL_REPORT_KIND: &str = "eval_report";
const LEARNING_ON_DEFAULT_PLAYBOOK_RECALL_MAX_PLAYBOOKS: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EvalVerdict {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalArmKind {
    LearningOn,
    LearningOff,
}

impl EvalArmKind {
    fn label(self) -> &'static str {
        match self {
            EvalArmKind::LearningOn => "learning_on",
            EvalArmKind::LearningOff => "learning_off",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalReport {
    pub eval_run_id: String,
    pub contract_id: String,
    pub repeats: usize,
    pub verdict: EvalVerdict,
    pub arms: Vec<EvalArmReport>,
    pub delta: EvalDelta,
    pub summary: String,
    pub report_ref: Option<LedgerRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalArmReport {
    pub arm: EvalArmKind,
    pub memory: MemoryConfig,
    pub repeats: usize,
    pub pass_count: usize,
    pub fail_count: usize,
    pub unverifiable_count: usize,
    pub pass_rate: f64,
    pub total_usage: BudgetSnapshot,
    pub child_runs: Vec<EvalChildRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalChildRun {
    pub repeat_index: usize,
    pub child_run_id: String,
    pub overall: ClauseVerdict,
    pub usage: BudgetSnapshot,
    pub verdict_ref: LedgerRef,
    pub parent_anchor: LedgerRef,
    pub child_root_hash: String,
    pub child_head_hash: String,
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalDelta {
    /// `learning_on.pass_rate - learning_off.pass_rate`.
    pub pass_rate: f64,
    /// `learning_on.total_usage.cost_usd - learning_off.total_usage.cost_usd`.
    pub cost_usd: f64,
    /// `learning_on.total_usage.steps - learning_off.total_usage.steps`.
    pub steps: i64,
    /// `learning_on.total_tokens - learning_off.total_tokens`.
    pub total_tokens: i64,
}

pub struct PairedEvalOptions {
    pub repeats: usize,
    pub budget_slice: BudgetLimits,
    pub base_cfg: DurableConfig,
    pub tools: Arc<ToolRegistry>,
    pub provider: Arc<dyn Provider>,
    pub reviewers: Vec<Arc<dyn Provider>>,
    pub memory_store: Arc<dyn MemoryStore>,
}

/// Run paired learning-on/off arms for the same contract and record an
/// `eval_report` entry in the parent ledger.
pub async fn run_paired_eval(
    ctx: &Ctx,
    store: &Store,
    contract: Arc<Contract>,
    options: PairedEvalOptions,
) -> Result<EvalReport, HarnessError> {
    if options.repeats == 0 {
        return Err(HarnessError::Other(
            "paired eval requires at least one repeat".into(),
        ));
    }

    let on_config = learning_on_config(&options.base_cfg.memory);
    let off_config = learning_off_config(&options.base_cfg.memory);

    let learning_on = run_arm(
        ctx,
        store,
        contract.clone(),
        &options,
        EvalArmKind::LearningOn,
        on_config,
    )
    .await?;
    let learning_off = run_arm(
        ctx,
        store,
        contract.clone(),
        &options,
        EvalArmKind::LearningOff,
        off_config,
    )
    .await?;

    let delta = EvalDelta::between(&learning_on, &learning_off);
    let verdict = if delta.pass_rate > 0.0
        || (delta.pass_rate == 0.0 && delta.cost_usd <= 0.0 && delta.total_tokens <= 0)
    {
        EvalVerdict::Pass
    } else {
        EvalVerdict::Fail
    };
    let summary = render_summary(
        &contract.contract.id,
        options.repeats,
        &learning_on,
        &learning_off,
        &delta,
        &verdict,
    );

    let mut report = EvalReport {
        eval_run_id: ctx.run_id.clone(),
        contract_id: contract.contract.id.clone(),
        repeats: options.repeats,
        verdict,
        arms: vec![learning_on, learning_off],
        delta,
        summary,
        report_ref: None,
    };

    let entry = store
        .append(
            &ctx.run_id,
            EVAL_REPORT_KIND,
            &serde_json::to_value(&report).map_err(|e| HarnessError::Other(e.to_string()))?,
        )
        .map_err(other)?;
    report.report_ref = Some(LedgerRef {
        session: ctx.run_id.clone(),
        seq: entry.seq,
    });
    Ok(report)
}

fn learning_on_config(base: &MemoryConfig) -> MemoryConfig {
    let defaults = MemoryConfig::default();
    let mut config = base.clone();
    config.extraction = LessonExtractionPolicy::On;
    config.verification = VerificationPolicy::Eager;
    if config.lesson_recall_max_lessons == 0 {
        config.lesson_recall_max_lessons = defaults.lesson_recall_max_lessons;
    }
    if config.lesson_recall_token_budget == 0 {
        config.lesson_recall_token_budget = defaults.lesson_recall_token_budget;
    }
    if config.playbook_recall_max_playbooks == 0 {
        config.playbook_recall_max_playbooks = LEARNING_ON_DEFAULT_PLAYBOOK_RECALL_MAX_PLAYBOOKS;
    }
    if config.playbook_recall_token_budget == 0 {
        config.playbook_recall_token_budget = defaults.playbook_recall_token_budget;
    }
    config
}

fn learning_off_config(base: &MemoryConfig) -> MemoryConfig {
    let mut config = base.clone();
    config.extraction = LessonExtractionPolicy::Off;
    config.verification = VerificationPolicy::Off;
    config.lesson_recall_max_lessons = 0;
    config.lesson_recall_token_budget = 0;
    config.playbook_recall_max_playbooks = 0;
    config.playbook_recall_token_budget = 0;
    config
}

async fn run_arm(
    ctx: &Ctx,
    store: &Store,
    contract: Arc<Contract>,
    options: &PairedEvalOptions,
    arm: EvalArmKind,
    memory: MemoryConfig,
) -> Result<EvalArmReport, HarnessError> {
    let mut child_runs = Vec::with_capacity(options.repeats);
    for repeat_index in 0..options.repeats {
        let mut child_options =
            ChildRunOptions::new(contract.executor_prompt(), options.budget_slice.clone())
                .with_scope(format!(
                    "eval:{}:contract:{}:repeat:{}",
                    arm.label(),
                    contract.contract.id,
                    repeat_index
                ));
        child_options.principal = Some(ctx.principal.clone());

        let result = run_eval_child(
            ctx,
            store,
            child_options,
            contract.clone(),
            options,
            arm,
            repeat_index,
            memory.clone(),
        )
        .await?;
        child_runs.push(child_to_report(repeat_index, result));
    }

    Ok(EvalArmReport::new(arm, memory, child_runs))
}

#[allow(clippy::too_many_arguments)]
async fn run_eval_child(
    ctx: &Ctx,
    store: &Store,
    child_options: ChildRunOptions,
    contract: Arc<Contract>,
    options: &PairedEvalOptions,
    arm: EvalArmKind,
    repeat_index: usize,
    memory: MemoryConfig,
) -> Result<VerifiedChildResult, HarnessError> {
    let provider = options.provider.clone();
    let reviewers = options.reviewers.clone();
    let tools = options.tools.clone();
    let memory_store = options.memory_store.clone();
    let mut cfg = options.base_cfg.clone();
    cfg.memory = memory.clone();
    cfg.memory_store = Some(memory_store.clone());
    cfg.goal_ledger = Some(GoalLedger::from_contract(
        contract.contract.intent.clone(),
        contract.done.iter().map(|d| d.desc.clone()).collect(),
        memory.goal_repin_every,
    ));

    run_child(ctx, store, child_options, move |child_ctx, child_store| {
        let contract = contract.clone();
        let provider = provider.clone();
        let reviewers = reviewers.clone();
        let tools = tools.clone();
        let memory_store = memory_store.clone();
        let cfg = cfg.clone();
        async move {
            let executor = match contract.cli_executor().map_err(other)? {
                Some(cli) => Executor::DelegatedCli(cli),
                None => Executor::InLoop(provider.clone()),
            };
            let close_learning = if cfg.memory.extraction == LessonExtractionPolicy::Off {
                None
            } else {
                Some(ContractCloseLearning {
                    memory_store,
                    extractor: provider.clone(),
                })
            };
            let verdict = delegation::run_contract_in_existing_session_with_close_learning(
                executor,
                reviewers,
                tools,
                &child_ctx,
                &child_store,
                &child_ctx.run_id,
                &contract,
                &cfg,
                close_learning,
            )
            .await?;
            child_store
                .append(
                    &child_ctx.run_id,
                    "eval_arm",
                    &json!({
                        "arm": arm,
                        "repeat_index": repeat_index,
                        "contract_id": contract.contract.id,
                        "memory": cfg.memory,
                    }),
                )
                .map_err(other)?;
            Ok(ChildRunOutput {
                verdict,
                artifacts: Vec::new(),
            })
        }
    })
    .await
}

fn child_to_report(repeat_index: usize, child: VerifiedChildResult) -> EvalChildRun {
    EvalChildRun {
        repeat_index,
        child_run_id: child.child_run_id,
        overall: child.verdict.overall,
        usage: child.usage,
        verdict_ref: child.verdict_ref,
        parent_anchor: child.parent_anchor,
        child_root_hash: child.child_root_hash,
        child_head_hash: child.child_head_hash,
        artifacts: child.artifacts,
    }
}

impl EvalArmReport {
    fn new(arm: EvalArmKind, memory: MemoryConfig, child_runs: Vec<EvalChildRun>) -> Self {
        let repeats = child_runs.len();
        let pass_count = child_runs
            .iter()
            .filter(|child| child.overall == ClauseVerdict::Pass)
            .count();
        let fail_count = child_runs
            .iter()
            .filter(|child| child.overall == ClauseVerdict::Fail)
            .count();
        let unverifiable_count = child_runs
            .iter()
            .filter(|child| child.overall == ClauseVerdict::Unverifiable)
            .count();
        let total_usage = sum_usage(child_runs.iter().map(|child| &child.usage));
        let pass_rate = if repeats == 0 {
            0.0
        } else {
            pass_count as f64 / repeats as f64
        };
        Self {
            arm,
            memory,
            repeats,
            pass_count,
            fail_count,
            unverifiable_count,
            pass_rate,
            total_usage,
            child_runs,
        }
    }
}

impl EvalDelta {
    fn between(on: &EvalArmReport, off: &EvalArmReport) -> Self {
        Self {
            pass_rate: on.pass_rate - off.pass_rate,
            cost_usd: on.total_usage.cost_usd - off.total_usage.cost_usd,
            steps: on.total_usage.steps as i64 - off.total_usage.steps as i64,
            total_tokens: on.total_usage.total_tokens() as i64
                - off.total_usage.total_tokens() as i64,
        }
    }
}

fn sum_usage<'a>(items: impl Iterator<Item = &'a BudgetSnapshot>) -> BudgetSnapshot {
    items.fold(BudgetSnapshot::default(), |mut acc, item| {
        acc.input_tokens += item.input_tokens;
        acc.output_tokens += item.output_tokens;
        acc.cost_usd += item.cost_usd;
        acc.steps += item.steps;
        acc
    })
}

fn render_summary(
    contract_id: &str,
    repeats: usize,
    on: &EvalArmReport,
    off: &EvalArmReport,
    delta: &EvalDelta,
    verdict: &EvalVerdict,
) -> String {
    format!(
        "EVAL {contract_id}: {verdict:?} over {repeats} repeat(s). \
         learning_on pass_rate={:.2} cost=${:.4} steps={} tokens={}; \
         learning_off pass_rate={:.2} cost=${:.4} steps={} tokens={}; \
         delta pass_rate={:+.2} cost={:+.4} steps={:+} tokens={:+}.",
        on.pass_rate,
        on.total_usage.cost_usd,
        on.total_usage.steps,
        on.total_usage.total_tokens(),
        off.pass_rate,
        off.total_usage.cost_usd,
        off.total_usage.steps,
        off.total_usage.total_tokens(),
        delta.pass_rate,
        delta.cost_usd,
        delta.steps,
        delta.total_tokens
    )
}

fn other(e: anyhow::Error) -> HarnessError {
    HarnessError::Other(e.to_string())
}
