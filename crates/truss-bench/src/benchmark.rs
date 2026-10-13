use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use truss_core::budget::{BudgetLimits, BudgetSnapshot};
use truss_core::contract::{BudgetSpec, ConstraintSpec, Contract, DoneClause, ExecutorSpec, Meta};
use truss_core::ctx::Ctx;
use truss_core::durable::DurableConfig;
use truss_core::error::HarnessError;
use truss_core::eval::{run_paired_eval, EvalArmKind, EvalDelta, EvalReport, PairedEvalOptions};
use truss_core::memory::{LedgerRef, MemoryStore};
use truss_core::provider::Provider;
use truss_core::store::Store;
use truss_core::tools::ToolRegistry;

#[derive(Debug, Error)]
pub enum BenchmarkError {
    #[error("task {task_id} could not be adapted: {reason}")]
    Adapt { task_id: String, reason: String },
    #[error("harness error: {0}")]
    Harness(#[from] HarnessError),
}

/// Common identity surface for an external benchmark item.
pub trait BenchmarkTask {
    fn id(&self) -> &str;
    fn title(&self) -> &str;
    fn intent(&self) -> &str;
}

/// Maps external benchmark items into Truss contracts.
///
/// The adapter owns the fit between a benchmark's grader and a Truss `done`
/// check. For SWE-bench-style tasks, the benchmark's test command becomes the
/// harness-owned shell check.
pub trait BenchmarkAdapter {
    type Task: BenchmarkTask;

    fn to_contract(&self, task: &Self::Task) -> Result<Contract, BenchmarkError>;

    fn ledger_record(
        &self,
        _task: &Self::Task,
        _report: &BenchmarkTaskReport,
    ) -> Result<Option<BenchmarkLedgerRecord>, BenchmarkError> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkLedgerRecord {
    pub kind: String,
    pub payload: serde_json::Value,
}

/// Local SWE-bench-style task: a repo fixture plus a shell/test command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SweBenchStyleTask {
    pub id: String,
    pub title: String,
    pub workdir: PathBuf,
    pub intent: String,
    pub test_cmd: String,
    pub expect_exit: i32,
}

impl SweBenchStyleTask {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        workdir: impl Into<PathBuf>,
        intent: impl Into<String>,
        test_cmd: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            workdir: workdir.into(),
            intent: intent.into(),
            test_cmd: test_cmd.into(),
            expect_exit: 0,
        }
    }
}

impl BenchmarkTask for SweBenchStyleTask {
    fn id(&self) -> &str {
        &self.id
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn intent(&self) -> &str {
        &self.intent
    }
}

#[derive(Debug, Clone, Default)]
pub struct SweBenchStyleAdapter;

impl BenchmarkAdapter for SweBenchStyleAdapter {
    type Task = SweBenchStyleTask;

    fn to_contract(&self, task: &Self::Task) -> Result<Contract, BenchmarkError> {
        if task.test_cmd.trim().is_empty() {
            return Err(BenchmarkError::Adapt {
                task_id: task.id.clone(),
                reason: "test_cmd cannot be empty".into(),
            });
        }

        let workdir =
            absolute_or_current(&task.workdir).map_err(|reason| BenchmarkError::Adapt {
                task_id: task.id.clone(),
                reason,
            })?;
        let check_cmd = format!("cd {} && {}", shell_quote_path(&workdir), task.test_cmd);

        Ok(Contract {
            contract: Meta {
                id: task.id.clone(),
                title: task.title.clone(),
                intent: task.intent.clone(),
            },
            done: vec![DoneClause {
                kind: "check".into(),
                desc: format!("Benchmark grader passes: {}", task.test_cmd),
                run: Some(check_cmd),
                expect_exit: Some(task.expect_exit),
                expect_contains: Vec::new(),
                reviewer_context: Vec::new(),
            }],
            budget: BudgetSpec {
                max_steps: Some(4),
                ..BudgetSpec::default()
            },
            constraints: ConstraintSpec {
                write_scope: Some(vec![workdir.clone()]),
                read_scope: Some(vec![workdir]),
                network: Some("none".into()),
                ..ConstraintSpec::default()
            },
            reviewers: Some(0),
            memory: Default::default(),
            sandbox: Default::default(),
            approval_gates: Default::default(),
            mcp: Default::default(),
            executor: ExecutorSpec::default(),
        })
    }
}

pub struct BenchmarkRunner {
    pub benchmark_id: String,
    pub repeats: usize,
    pub budget_slice: BudgetLimits,
    pub base_cfg: DurableConfig,
    pub tools: Arc<ToolRegistry>,
    pub provider: Arc<dyn Provider>,
    pub reviewers: Vec<Arc<dyn Provider>>,
    pub memory_store: Arc<dyn MemoryStore>,
}

impl BenchmarkRunner {
    pub fn new(
        benchmark_id: impl Into<String>,
        provider: Arc<dyn Provider>,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Self {
        Self {
            benchmark_id: benchmark_id.into(),
            repeats: 1,
            budget_slice: BudgetLimits {
                max_steps: Some(4),
                ..BudgetLimits::default()
            },
            base_cfg: DurableConfig::default(),
            tools: Arc::new(ToolRegistry::new()),
            provider,
            reviewers: Vec::new(),
            memory_store,
        }
    }

    pub async fn run<A>(
        &self,
        ctx: &Ctx,
        store: &Store,
        adapter: &A,
        tasks: &[A::Task],
    ) -> Result<BenchmarkReport, BenchmarkError>
    where
        A: BenchmarkAdapter,
    {
        let mut reports = Vec::with_capacity(tasks.len());
        for task in tasks {
            let contract = Arc::new(adapter.to_contract(task)?);
            let eval_report = run_paired_eval(
                ctx,
                store,
                contract,
                PairedEvalOptions {
                    repeats: self.repeats,
                    budget_slice: self.budget_slice.clone(),
                    base_cfg: self.base_cfg.clone(),
                    tools: self.tools.clone(),
                    provider: self.provider.clone(),
                    reviewers: self.reviewers.clone(),
                    memory_store: self.memory_store.clone(),
                },
            )
            .await?;
            let mut report = BenchmarkTaskReport::from_eval(task, eval_report);
            if let Some(record) = adapter.ledger_record(task, &report)? {
                let entry = store
                    .append(&ctx.run_id, &record.kind, &record.payload)
                    .map_err(|e| BenchmarkError::Harness(HarnessError::Other(e.to_string())))?;
                report.ledger_ref = Some(LedgerRef {
                    session: ctx.run_id.clone(),
                    seq: entry.seq,
                });
            }
            reports.push(report);
        }
        Ok(BenchmarkReport::new(self.benchmark_id.clone(), reports))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkReport {
    pub benchmark_id: String,
    pub task_count: usize,
    pub pass_count: usize,
    pub fail_count: usize,
    pub total_usage: BudgetSnapshot,
    pub tasks: Vec<BenchmarkTaskReport>,
}

impl BenchmarkReport {
    fn new(benchmark_id: String, tasks: Vec<BenchmarkTaskReport>) -> Self {
        let task_count = tasks.len();
        let pass_count = tasks.iter().filter(|task| task.passed).count();
        let fail_count = task_count - pass_count;
        let total_usage = tasks
            .iter()
            .fold(BudgetSnapshot::default(), |mut acc, task| {
                acc.input_tokens += task.usage.input_tokens;
                acc.output_tokens += task.usage.output_tokens;
                acc.cost_usd += task.usage.cost_usd;
                acc.steps += task.usage.steps;
                acc
            });
        Self {
            benchmark_id,
            task_count,
            pass_count,
            fail_count,
            total_usage,
            tasks,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkTaskReport {
    pub task_id: String,
    pub title: String,
    pub verdict: BenchmarkVerdict,
    pub passed: bool,
    pub usage: BudgetSnapshot,
    pub delta: EvalDelta,
    pub ledger_ref: Option<LedgerRef>,
    pub eval_report: EvalReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkVerdict {
    Pass,
    Fail,
}

impl BenchmarkTaskReport {
    fn from_eval(task: &impl BenchmarkTask, eval_report: EvalReport) -> Self {
        let usage = eval_report
            .arms
            .iter()
            .flat_map(|arm| arm.child_runs.iter())
            .fold(BudgetSnapshot::default(), |mut acc, child| {
                acc.input_tokens += child.usage.input_tokens;
                acc.output_tokens += child.usage.output_tokens;
                acc.cost_usd += child.usage.cost_usd;
                acc.steps += child.usage.steps;
                acc
            });
        let passed = eval_report
            .arms
            .iter()
            .find(|arm| arm.arm == EvalArmKind::LearningOn)
            .is_some_and(|arm| arm.pass_count == arm.repeats && arm.repeats > 0);
        let verdict = if passed {
            BenchmarkVerdict::Pass
        } else {
            BenchmarkVerdict::Fail
        };
        Self {
            task_id: task.id().to_string(),
            title: task.title().to_string(),
            verdict,
            passed,
            usage,
            delta: eval_report.delta.clone(),
            ledger_ref: None,
            eval_report,
        }
    }
}

fn absolute_or_current(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|e| e.to_string())
}

fn shell_quote_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}
