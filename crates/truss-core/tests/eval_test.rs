use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use truss_core::budget::BudgetSnapshot;
use truss_core::contract::{BudgetSpec, ConstraintSpec, Contract, DoneClause, ExecutorSpec, Meta};
use truss_core::ctx::Ctx;
use truss_core::delegation::{apply_memory_verdict, ClauseResult, ClauseVerdict, Verdict};
use truss_core::durable::DurableConfig;
use truss_core::error::ProviderError;
use truss_core::eval::{run_paired_eval, EvalArmKind, EvalVerdict, PairedEvalOptions};
use truss_core::memory::{
    EntryKind, LedgerRef, LessonExtractionPolicy, MemoryConfig, MemoryDraft, MemoryStore,
    SqliteMemoryStore, VerificationPolicy,
};
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::Store;
use truss_core::tools::ToolRegistry;

#[derive(Clone, Default)]
struct EvalProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl Provider for EvalProvider {
    fn name(&self) -> &str {
        "eval-provider"
    }

    fn default_model(&self) -> &str {
        "gpt-4o-mini"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let system = req.system.clone().unwrap_or_default();
        let prompt = request_text(req);
        let body = if system.contains("Extract only durable") {
            r#"{"lessons":[]}"#.to_string()
        } else if prompt.contains("[L2 verified learned lessons]") {
            "used verified lesson".to_string()
        } else {
            "missed verified lesson".to_string()
        };
        Ok(text(&body, 11, 7))
    }
}

struct EvalReviewer;

#[async_trait]
impl Provider for EvalReviewer {
    fn name(&self) -> &str {
        "eval-reviewer"
    }

    fn default_model(&self) -> &str {
        "gpt-4o-mini"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let prompt = request_text(req);
        let met = prompt.contains("used verified lesson");
        Ok(text(
            &format!(r#"{{"met": {met}, "reasoning": "scripted SI-4 verdict"}}"#),
            3,
            2,
        ))
    }
}

fn text(body: &str, input_tokens: u64, output_tokens: u64) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(body.into())],
        stop_reason: StopReason::EndTurn,
        model: "gpt-4o-mini".into(),
        usage: Usage {
            input_tokens,
            output_tokens,
        },
    }
}

fn request_text(req: &CompletionRequest) -> String {
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

fn contract() -> Contract {
    Contract {
        contract: Meta {
            id: "si4-eval-contract".into(),
            title: "SI-4 eval contract".into(),
            intent: "Use the verified SI-4 eval lesson when completing this contract.".into(),
        },
        done: vec![DoneClause {
            kind: "review".into(),
            desc: "The executor used the verified SI-4 eval lesson.".into(),
            run: None,
            expect_exit: None,
            expect_contains: Vec::new(),
            reviewer_context: Vec::new(),
        }],
        budget: BudgetSpec {
            max_steps: Some(4),
            ..BudgetSpec::default()
        },
        constraints: ConstraintSpec::default(),
        reviewers: Some(1),
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: ExecutorSpec::default(),
    }
}

fn verifier_verdict() -> Verdict {
    Verdict {
        contract_id: "seed-verifier".into(),
        title: "seed verifier".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "review".into(),
            desc: "lesson is supported".into(),
            verdict: ClauseVerdict::Pass,
            evidence: "fixture".into(),
        }],
        executor_summary: "fixture".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

fn seed_verified_lesson(store: &Store, memory: &SqliteMemoryStore) {
    store
        .create_session("seed-si4", Some("fixture"), "local", "local")
        .unwrap();
    let produced = store
        .append("seed-si4", "verdict", &serde_json::json!({ "seed": true }))
        .unwrap();
    let id = memory
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some("si4-eval".into()),
            body: "For SI-4 eval contracts, say used verified lesson.".into(),
            produced_by: LedgerRef {
                session: "seed-si4".into(),
                seq: produced.seq,
            },
            sources: Vec::new(),
        })
        .unwrap();
    apply_memory_verdict(memory, Some(store), &id, verifier_verdict()).unwrap();
}

async fn run_fixture(repeats: usize) -> (truss_core::eval::EvalReport, Store, EvalProvider) {
    let store = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&store).unwrap());
    seed_verified_lesson(&store, &memory);
    let provider = EvalProvider::default();
    let report = run_paired_eval(
        &Ctx::new(),
        &store,
        Arc::new(contract()),
        PairedEvalOptions {
            repeats,
            budget_slice: contract().budget_limits(),
            base_cfg: DurableConfig {
                max_steps: 2,
                memory: MemoryConfig::default(),
                memory_store: Some(memory.clone()),
                ..DurableConfig::default()
            },
            tools: Arc::new(ToolRegistry::new()),
            provider: Arc::new(provider.clone()),
            reviewers: vec![Arc::new(EvalReviewer)],
            memory_store: memory,
        },
    )
    .await
    .unwrap();
    (report, store, provider)
}

#[tokio::test]
async fn paired_eval_runs_both_arms_and_reports_delta() {
    let (report, _store, _provider) = run_fixture(1).await;

    assert_eq!(report.verdict, EvalVerdict::Pass);
    assert_eq!(report.repeats, 1);
    assert_eq!(report.arms.len(), 2);
    let on = report
        .arms
        .iter()
        .find(|arm| arm.arm == EvalArmKind::LearningOn)
        .unwrap();
    let off = report
        .arms
        .iter()
        .find(|arm| arm.arm == EvalArmKind::LearningOff)
        .unwrap();
    assert_eq!(on.pass_count, 1);
    assert_eq!(off.fail_count, 1);
    assert_eq!(report.delta.pass_rate, 1.0);
    assert!(on.total_usage.cost_usd > 0.0);
    assert!(on.total_usage.steps > 0);
    assert!(on.total_usage.total_tokens() > 0);
    assert!(report.summary.contains("learning_on pass_rate=1.00"));
}

#[tokio::test]
async fn paired_eval_flips_only_memory_learning_knobs() {
    let (report, _store, provider) = run_fixture(1).await;
    let on = report
        .arms
        .iter()
        .find(|arm| arm.arm == EvalArmKind::LearningOn)
        .unwrap();
    let off = report
        .arms
        .iter()
        .find(|arm| arm.arm == EvalArmKind::LearningOff)
        .unwrap();

    assert_eq!(on.memory.extraction, LessonExtractionPolicy::On);
    assert_eq!(on.memory.verification, VerificationPolicy::Eager);
    assert!(on.memory.lesson_recall_max_lessons > 0);
    assert!(on.memory.lesson_recall_token_budget > 0);
    assert!(on.memory.playbook_recall_max_playbooks > 0);
    assert!(on.memory.playbook_recall_token_budget > 0);
    assert_eq!(off.memory.extraction, LessonExtractionPolicy::Off);
    assert_eq!(off.memory.verification, VerificationPolicy::Off);
    assert_eq!(off.memory.lesson_recall_max_lessons, 0);
    assert_eq!(off.memory.lesson_recall_token_budget, 0);
    assert_eq!(off.memory.playbook_recall_max_playbooks, 0);
    assert_eq!(off.memory.playbook_recall_token_budget, 0);

    let executor_requests = provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|req| {
            !req.system
                .as_deref()
                .unwrap_or_default()
                .contains("Extract only durable")
        })
        .map(request_text)
        .collect::<Vec<_>>();
    assert!(
        executor_requests[0].contains("[L2 verified learned lessons]"),
        "{}",
        executor_requests[0]
    );
    assert!(
        !executor_requests[1].contains("[L2 verified learned lessons]"),
        "{}",
        executor_requests[1]
    );
}

#[tokio::test]
async fn eval_report_serializes_round_trip() {
    let (report, _store, _provider) = run_fixture(1).await;
    let encoded = serde_json::to_string(&report).unwrap();
    let decoded: truss_core::eval::EvalReport = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, report);
}

#[tokio::test]
async fn eval_report_references_verifiable_child_runs() {
    let (report, store, _provider) = run_fixture(2).await;

    let report_ref = report.report_ref.as_ref().expect("report ref");
    assert_eq!(report_ref.session, report.eval_run_id);
    assert!(store.verify_chain(&report.eval_run_id).unwrap().is_ok());

    for arm in &report.arms {
        assert_eq!(arm.child_runs.len(), 2);
        for child in &arm.child_runs {
            assert!(!child.child_run_id.is_empty());
            assert_eq!(child.verdict_ref.session, child.child_run_id);
            assert_eq!(child.parent_anchor.session, report.eval_run_id);
            assert!(store.verify_chain(&child.child_run_id).unwrap().is_ok());
        }
    }
}
