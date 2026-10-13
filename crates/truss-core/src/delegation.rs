//! Verified delegation — the north-star.
//!
//! Run a [`crate::contract::Contract`] to a **verdict backed by evidence the
//! executor did not produce**:
//!
//! 1. an executor agent does the work (a durable, signed session);
//! 2. the harness independently runs each `check` clause (shell commands it owns,
//!    not the executor's) and convenes a **panel of N adversarial no-stake
//!    reviewers** (in parallel, each attacking from a different lens) to judge each
//!    `review` clause — a strict majority must say "met" to pass;
//! 3. a verdict (PASS / FAIL / UNVERIFIABLE per clause) is recorded as a signed
//!    journal entry — the session head hash is the portable evidence bundle.
//!
//! The human reviews the verdict, not the transcript. The executor never grades
//! its own homework: checks run in an environment it didn't control, and the
//! reviewers are separate model/contexts prompted to refute. One lenient reviewer
//! can't pass bad work — it takes a quorum.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::budget::BudgetSnapshot;
use crate::contract::{BudgetSpec, ConstraintSpec, Contract, DoneClause, ExecutorSpec, Meta};
use crate::ctx::Ctx;
use crate::durable::{self, DurableConfig, DurableOutcome};
use crate::error::HarnessError;
use crate::events::AgentEvent;
use crate::executor::Executor;
use crate::memory::{
    EntryKind, LedgerRef, LessonExtractionPolicy, MemoryDraft, MemoryId, MemoryStore, Trust,
};
use crate::provider::Provider;
use crate::store::Store;
use crate::tools::ToolRegistry;

/// Optional contract-close learning runtime. Policy still lives in
/// [`DurableConfig::memory`]; this only binds the store and model dependency
/// needed when extraction is enabled.
#[derive(Clone)]
pub struct ContractCloseLearning {
    pub memory_store: Arc<dyn MemoryStore>,
    pub extractor: Arc<dyn Provider>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ClauseVerdict {
    Pass,
    Fail,
    Unverifiable,
}

impl ClauseVerdict {
    pub fn glyph(&self) -> char {
        match self {
            ClauseVerdict::Pass => '✓',
            ClauseVerdict::Fail => '✗',
            ClauseVerdict::Unverifiable => '?',
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClauseResult {
    pub kind: String,
    pub desc: String,
    pub verdict: ClauseVerdict,
    pub evidence: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ReviewedDiff {
    pub source: String,
    pub full_sha256: String,
    pub reviewed_sha256: String,
    pub full_bytes: usize,
    pub reviewed_bytes: usize,
    pub truncated: bool,
    pub content: String,
}

impl ReviewedDiff {
    pub fn empty(source: impl Into<String>) -> Self {
        let content = String::new();
        let hash = sha256_hex(content.as_bytes());
        Self {
            source: source.into(),
            full_sha256: hash.clone(),
            reviewed_sha256: hash,
            full_bytes: 0,
            reviewed_bytes: 0,
            truncated: false,
            content,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    pub contract_id: String,
    pub title: String,
    pub overall: ClauseVerdict,
    pub results: Vec<ClauseResult>,
    pub executor_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_diff: Option<ReviewedDiff>,
    pub usage: BudgetSnapshot,
}

const MAX_REVIEW_DIFF_BYTES: usize = 20_000;

/// Run a contract end-to-end and return the evidence-backed verdict.
pub async fn run_contract(
    executor: Executor,
    reviewers: Vec<Arc<dyn Provider>>,
    tools: Arc<ToolRegistry>,
    ctx: &Ctx,
    store: &Store,
    contract: &Contract,
    exec_cfg: &DurableConfig,
) -> Result<Verdict, HarnessError> {
    run_contract_with_close_learning(
        executor, reviewers, tools, ctx, store, contract, exec_cfg, None,
    )
    .await
}

/// Run a contract and optionally extract unverified lessons after the signed
/// verdict lands.
#[allow(clippy::too_many_arguments)]
pub async fn run_contract_with_close_learning(
    executor: Executor,
    reviewers: Vec<Arc<dyn Provider>>,
    tools: Arc<ToolRegistry>,
    ctx: &Ctx,
    store: &Store,
    contract: &Contract,
    exec_cfg: &DurableConfig,
    close_learning: Option<ContractCloseLearning>,
) -> Result<Verdict, HarnessError> {
    run_contract_in_session_with_close_learning(
        executor,
        reviewers,
        tools,
        ctx,
        store,
        &contract.contract.id,
        false,
        contract,
        exec_cfg,
        close_learning,
    )
    .await
}

/// Run a contract inside an already-created session.
///
/// This is used by child-run based orchestration: the child ledger exists before
/// the durable executor starts, so the caller asks delegation to append the
/// normal `run_started` boundary into that existing signed session instead of
/// creating a separate contract-id session.
#[allow(clippy::too_many_arguments)]
pub async fn run_contract_in_existing_session_with_close_learning(
    executor: Executor,
    reviewers: Vec<Arc<dyn Provider>>,
    tools: Arc<ToolRegistry>,
    ctx: &Ctx,
    store: &Store,
    session_id: &str,
    contract: &Contract,
    exec_cfg: &DurableConfig,
    close_learning: Option<ContractCloseLearning>,
) -> Result<Verdict, HarnessError> {
    run_contract_in_session_with_close_learning(
        executor,
        reviewers,
        tools,
        ctx,
        store,
        session_id,
        true,
        contract,
        exec_cfg,
        close_learning,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_contract_in_session_with_close_learning(
    executor: Executor,
    reviewers: Vec<Arc<dyn Provider>>,
    tools: Arc<ToolRegistry>,
    ctx: &Ctx,
    store: &Store,
    session_id: &str,
    existing_session: bool,
    contract: &Contract,
    exec_cfg: &DurableConfig,
    close_learning: Option<ContractCloseLearning>,
) -> Result<Verdict, HarnessError> {
    if existing_session && store.get_session(session_id).map_err(other)?.is_none() {
        return Err(HarnessError::Other(format!(
            "existing contract session {session_id} not found"
        )));
    }

    let diff_baseline = capture_diff_baseline(contract);

    // ── 1. Execute ─────────────────────────────────────────────────────────
    // InLoop: Truss owns the loop (durable, signed, journaled per-step).
    // DelegatedCli: a vendor agent owns the inner loop on its own subscription;
    // we capture the result and verify at the contract boundary (ADR 0003).
    let (executor_summary, delegated_session) = match executor {
        Executor::InLoop(provider) => {
            if existing_session {
                store
                    .append(
                        session_id,
                        "run_started",
                        &json!({
                            "prompt": contract.executor_prompt(),
                            "model": exec_cfg
                                .model
                                .clone()
                                .unwrap_or_else(|| provider.default_model().to_string()),
                            "system": exec_cfg.system.clone(),
                        }),
                    )
                    .map_err(other)?;
            }
            let exec_outcome = durable::run(
                provider,
                tools,
                ctx,
                store,
                session_id,
                exec_cfg,
                (!existing_session).then(|| contract.executor_prompt()),
            )
            .await?;
            match exec_outcome {
                DurableOutcome::Completed { final_text } => (final_text, false),
                DurableOutcome::Suspended { await_desc, .. } => {
                    (format!("[executor suspended awaiting {await_desc}]"), false)
                }
                DurableOutcome::StillWaiting { await_desc } => {
                    (format!("[executor still waiting on {await_desc}]"), false)
                }
            }
        }
        Executor::DelegatedCli(cli) => {
            if store.get_session(session_id).map_err(other)?.is_none() {
                let model = format!("delegated-cli:{}", cli.command);
                store
                    .create_session(
                        session_id,
                        Some(&model),
                        &ctx.principal.tenant,
                        &ctx.principal.user,
                    )
                    .map_err(other)?;
                store
                    .append(
                        session_id,
                        "run_started",
                        &json!({
                            "prompt": contract.executor_prompt(),
                            "model": model,
                            "system": exec_cfg.system.clone(),
                        }),
                    )
                    .map_err(other)?;
            }
            if existing_session {
                store
                    .append(
                        session_id,
                        "run_started",
                        &json!({
                            "prompt": contract.executor_prompt(),
                            "model": format!("delegated-cli:{}", cli.command),
                            "system": exec_cfg.system.clone(),
                        }),
                    )
                    .map_err(other)?;
            }
            let summary = cli.run(ctx, &contract.executor_prompt()).await?;
            // The inner steps aren't ours to journal, but the boundary is: record
            // the delegated execution into the same signed ledger.
            store
                .append(
                    session_id,
                    "delegated_exec",
                    &json!({ "command": cli.command, "summary_len": summary.len() }),
                )
                .map_err(other)?;
            (summary, true)
        }
    };

    let reviewed_diff = match diff_baseline {
        Ok(baseline) => baseline
            .capture()
            .unwrap_or_else(|reason| ReviewedDiff::empty(format!("unavailable: {reason}"))),
        Err(reason) => ReviewedDiff::empty(format!("unavailable: {reason}")),
    };
    store
        .append(
            session_id,
            "work_diff",
            &json!({
                "source": reviewed_diff.source,
                "full_sha256": reviewed_diff.full_sha256,
                "reviewed_sha256": reviewed_diff.reviewed_sha256,
                "full_bytes": reviewed_diff.full_bytes,
                "reviewed_bytes": reviewed_diff.reviewed_bytes,
                "truncated": reviewed_diff.truncated,
            }),
        )
        .map_err(other)?;

    // Record the contract into the same signed ledger.
    store
        .append(
            session_id,
            "contract",
            &json!({
                "id": contract.contract.id,
                "title": contract.contract.title,
                "intent": contract.contract.intent,
            }),
        )
        .map_err(other)?;

    // ── 2. Evidence: checks (harness-run) + reviews (no-stake model) ──────
    let mut results = Vec::new();
    for clause in &contract.done {
        let result = match clause.kind.as_str() {
            "check" => run_check(clause).await,
            "review" => {
                run_review_panel(
                    &reviewers,
                    ctx,
                    contract,
                    clause,
                    &executor_summary,
                    Some(&reviewed_diff),
                )
                .await
            }
            other_kind => ClauseResult {
                kind: other_kind.to_string(),
                desc: clause.desc.clone(),
                verdict: ClauseVerdict::Unverifiable,
                evidence: format!("unknown clause kind '{other_kind}'"),
            },
        };
        results.push(result);
    }

    // ── 3. Overall verdict ────────────────────────────────────────────────
    let overall = if results.iter().any(|r| r.verdict == ClauseVerdict::Fail) {
        ClauseVerdict::Fail
    } else if results
        .iter()
        .any(|r| r.verdict == ClauseVerdict::Unverifiable)
    {
        ClauseVerdict::Unverifiable
    } else {
        ClauseVerdict::Pass
    };

    let verdict = Verdict {
        contract_id: contract.contract.id.clone(),
        title: contract.contract.title.clone(),
        overall: overall.clone(),
        results,
        executor_summary,
        reviewed_diff: Some(reviewed_diff.clone()),
        usage: ctx.budget.snapshot(),
    };

    // Record the verdict (signed).
    let verdict_entry = store
        .append(
            session_id,
            "verdict",
            &json!({
                "overall": format!("{:?}", verdict.overall),
                "reviewed_diff": verdict.reviewed_diff.as_ref().map(|diff| json!({
                    "source": diff.source,
                    "full_sha256": diff.full_sha256,
                    "reviewed_sha256": diff.reviewed_sha256,
                    "full_bytes": diff.full_bytes,
                    "reviewed_bytes": diff.reviewed_bytes,
                    "truncated": diff.truncated,
                })),
                "clauses": verdict.results.iter().map(|r| json!({
                    "kind": r.kind, "desc": r.desc,
                    "verdict": format!("{:?}", r.verdict), "evidence": r.evidence,
                })).collect::<Vec<_>>(),
            }),
        )
        .map_err(other)?;
    let verdict_ref = LedgerRef {
        session: session_id.to_string(),
        seq: verdict_entry.seq,
    };
    if delegated_session {
        store.set_status(session_id, "completed").map_err(other)?;
    }

    if let Some(close_learning) = close_learning {
        extract_lessons_at_close(LessonExtractionContext {
            learning: &close_learning,
            reviewers: &reviewers,
            ctx,
            contract,
            verdict: &verdict,
            verdict_ref,
            config: &exec_cfg.memory,
            journal: store,
        })
        .await?;
    }

    Ok(verdict)
}

struct LessonExtractionContext<'a> {
    learning: &'a ContractCloseLearning,
    reviewers: &'a [Arc<dyn Provider>],
    ctx: &'a Ctx,
    contract: &'a Contract,
    verdict: &'a Verdict,
    verdict_ref: LedgerRef,
    config: &'a crate::memory::MemoryConfig,
    journal: &'a Store,
}

async fn extract_lessons_at_close(args: LessonExtractionContext<'_>) -> Result<(), HarnessError> {
    if args.config.extraction == LessonExtractionPolicy::Off {
        return Ok(());
    }

    let prompt = lesson_extraction_prompt(args.contract, args.verdict, args.config.extraction);
    let mut req =
        crate::provider::CompletionRequest::new(vec![crate::message::Message::user(prompt)]);
    req.system = Some(
        "Extract only durable, generalizable engineering lessons from the verified run. \
         Return ONLY JSON: {\"lessons\":[{\"key\":\"stable-slug\",\"body\":\"lesson\",\
         \"salience\":0.0}]}."
            .into(),
    );
    req.max_tokens = 1024;
    req.model = Some(args.learning.extractor.default_model().to_string());
    req.temperature = Some(0.0);

    let resp = args
        .learning
        .extractor
        .complete(args.ctx, &req)
        .await
        .map_err(HarnessError::Provider)?;
    let text = crate::message::Message {
        role: crate::message::Role::Assistant,
        content: resp.content,
    }
    .text();

    for lesson in parse_extracted_lessons(&text) {
        if args.config.extraction == LessonExtractionPolicy::ModelScored
            && lesson.salience < args.config.extraction_min_salience
        {
            continue;
        }
        let draft = MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some(lesson.key.clone()),
            body: lesson.body,
            produced_by: args.verdict_ref.clone(),
            sources: vec![args.verdict_ref.clone()],
        };
        let id = args
            .learning
            .memory_store
            .write(draft)
            .map_err(memory_other)?;
        args.ctx.emit(AgentEvent::MemoryWrite {
            id: id.0.clone(),
            path: format!("/memories/lessons/{}", lesson.key),
            kind: EntryKind::Lesson,
            key: Some(lesson.key),
            trust: Trust::Unverified,
            produced_by: args.verdict_ref.clone(),
        });
        if args.config.verify_on_write(EntryKind::Lesson) && !args.reviewers.is_empty() {
            let entry = args.learning.memory_store.get(&id).map_err(memory_other)?;
            let evidence =
                lesson_promotion_evidence(args.contract, args.verdict, &args.verdict_ref);
            let panel_verdict =
                run_memory_claim_with_evidence(args.reviewers, args.ctx, &entry, Some(&evidence))
                    .await;
            apply_memory_verdict(
                &*args.learning.memory_store,
                Some(args.journal),
                &id,
                panel_verdict,
            )?;
        }
    }

    Ok(())
}

fn lesson_promotion_evidence(
    contract: &Contract,
    verdict: &Verdict,
    verdict_ref: &LedgerRef,
) -> String {
    format!(
        "Producing ledger verdict: {}#{}\nContract id: {}\nTitle: {}\nIntent:\n{}\n\n\
         Overall verdict: {:?}\nExecutor summary:\n{}\n\nClause evidence:\n{}",
        verdict_ref.session,
        verdict_ref.seq,
        contract.contract.id,
        contract.contract.title,
        contract.contract.intent,
        verdict.overall,
        verdict.executor_summary,
        verdict
            .results
            .iter()
            .map(|r| format!("- {:?} {}: {}", r.verdict, r.desc, r.evidence))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn lesson_extraction_prompt(
    contract: &Contract,
    verdict: &Verdict,
    policy: LessonExtractionPolicy,
) -> String {
    let scoring = match policy {
        LessonExtractionPolicy::ModelScored => {
            "Score salience from 0.0 to 1.0. Include only genuinely reusable lessons."
        }
        LessonExtractionPolicy::On | LessonExtractionPolicy::Off => {
            "Include salience if useful; it will not gate writes in this mode."
        }
    };
    format!(
        "Contract id: {}\nTitle: {}\nIntent:\n{}\n\nOverall verdict: {:?}\n\
         Executor summary:\n{}\n\nClause evidence:\n{}\n\n{scoring}\n\
         Return up to 5 lessons. Lessons must be concise, generalizable, and not \
         restate task-specific facts.",
        contract.contract.id,
        contract.contract.title,
        contract.contract.intent,
        verdict.overall,
        verdict.executor_summary,
        verdict
            .results
            .iter()
            .map(|r| format!("- {:?} {}: {}", r.verdict, r.desc, r.evidence))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

#[derive(Debug)]
struct ExtractedLesson {
    key: String,
    body: String,
    salience: f32,
}

fn parse_extracted_lessons(text: &str) -> Vec<ExtractedLesson> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return Vec::new();
    };
    let Some(items) = value.get("lessons").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let key = item.get("key")?.as_str()?.trim();
            let body = item.get("body")?.as_str()?.trim();
            if key.is_empty() || body.is_empty() {
                return None;
            }
            let salience = item
                .get("salience")
                .and_then(|v| v.as_f64())
                .unwrap_or(1.0)
                .clamp(0.0, 1.0) as f32;
            Some(ExtractedLesson {
                key: key.to_string(),
                body: body.to_string(),
                salience,
            })
        })
        .take(5)
        .collect()
}

/// Run the same no-stake reviewer panel used for contract `review` clauses
/// against a memory claim. This deliberately returns the contract-layer
/// [`Verdict`] so memory promotion and contract completion share one verdict
/// type and one panel implementation.
pub async fn run_memory_claim(
    reviewers: &[Arc<dyn Provider>],
    ctx: &Ctx,
    entry: &crate::memory::MemoryEntry,
) -> Verdict {
    run_memory_claim_with_evidence(reviewers, ctx, entry, None).await
}

/// Run the shared reviewer panel against a memory claim plus optional evidence
/// context from the ledger event that produced it.
pub async fn run_memory_claim_with_evidence(
    reviewers: &[Arc<dyn Provider>],
    ctx: &Ctx,
    entry: &crate::memory::MemoryEntry,
    evidence: Option<&str>,
) -> Verdict {
    let intent = "Decide whether this model-authored memory entry is accurate \
                  and supported enough to promote to trusted durable memory.";
    let contract = Contract {
        contract: Meta {
            id: format!("memory-{}", entry.id.0),
            title: "Memory claim verification".into(),
            intent: intent.into(),
        },
        done: vec![DoneClause {
            kind: "review".into(),
            desc: "The memory claim is accurate, non-misleading, and safe to rely on later.".into(),
            run: None,
            expect_exit: None,
            expect_contains: Vec::new(),
            reviewer_context: Vec::new(),
        }],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec::default(),
        reviewers: Some(reviewers.len().max(1)),
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: ExecutorSpec::default(),
    };
    let mut work_summary = format!(
        "Memory kind: {:?}\nMemory key: {}\nProduced by: {}#{}\nSources: {}\nClaim:\n{}",
        entry.kind,
        entry.key.as_deref().unwrap_or("<none>"),
        entry.provenance.produced_by.session,
        entry.provenance.produced_by.seq,
        entry
            .provenance
            .sources
            .iter()
            .map(|source| format!("{}#{}", source.session, source.seq))
            .collect::<Vec<_>>()
            .join(", "),
        entry.body
    );
    if let Some(evidence) = evidence {
        work_summary.push_str("\n\nEvidence:\n");
        work_summary.push_str(evidence);
    }
    let result = run_review_panel(
        reviewers,
        ctx,
        &contract,
        &contract.done[0],
        &work_summary,
        None,
    )
    .await;
    let overall = result.verdict.clone();

    Verdict {
        contract_id: contract.contract.id,
        title: contract.contract.title,
        overall,
        results: vec![result],
        executor_summary: work_summary,
        reviewed_diff: None,
        usage: ctx.budget.snapshot(),
    }
}

/// Apply the panel verdict to a memory entry using the M3 trust-tier machinery.
/// Passing verdicts promote and attach the signed verdict to provenance; failed
/// or unverifiable verdicts reject. When a durable journal is supplied, the same
/// signed `memory_verdict` event pattern used by the memory tool is recorded.
pub fn apply_memory_verdict(
    memory_store: &dyn MemoryStore,
    journal: Option<&Store>,
    id: &MemoryId,
    verdict: Verdict,
) -> Result<(), HarnessError> {
    let entry = memory_store.get(id).map_err(memory_other)?;
    let verdict_ref = if let Some(journal) = journal {
        let appended = journal
            .append(
                &entry.provenance.produced_by.session,
                "memory_verdict",
                &json!({
                    "memory_id": id.0.as_str(),
                    "overall": format!("{:?}", verdict.overall),
                    "verdict": verdict.clone(),
                }),
            )
            .map_err(other)?;
        Some(LedgerRef {
            session: entry.provenance.produced_by.session.clone(),
            seq: appended.seq,
        })
    } else {
        None
    };
    match (&verdict.overall, verdict_ref) {
        (ClauseVerdict::Pass, Some(verdict_ref)) => memory_store
            .promote_with_verdict_ref(id, verdict.clone(), verdict_ref)
            .map_err(memory_other),
        (ClauseVerdict::Pass, None) => memory_store
            .promote(id, verdict.clone())
            .map_err(memory_other),
        (ClauseVerdict::Fail | ClauseVerdict::Unverifiable, Some(verdict_ref)) => memory_store
            .reject_with_verdict_ref(id, verdict.clone(), verdict_ref)
            .map_err(memory_other),
        (ClauseVerdict::Fail | ClauseVerdict::Unverifiable, None) => memory_store
            .reject(id, verdict.clone())
            .map_err(memory_other),
    }?;
    Ok(())
}

/// Run a `check` clause: execute its command, compare to `expect_*`.
async fn run_check(clause: &DoneClause) -> ClauseResult {
    let cmd = match &clause.run {
        Some(c) => c.clone(),
        None => {
            return ClauseResult {
                kind: "check".into(),
                desc: clause.desc.clone(),
                verdict: ClauseVerdict::Unverifiable,
                evidence: "check clause has no `run` command".into(),
            }
        }
    };

    let output = Command::new("sh").arg("-c").arg(&cmd).output().await;
    let (verdict, evidence) = match output {
        Ok(out) => {
            let code = out.status.code();
            let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));

            let exit_ok = clause.expect_exit.is_none_or(|c| code == Some(c));
            let contains_ok = clause
                .expect_contains
                .iter()
                .all(|needle| combined.contains(needle));

            let passed = exit_ok && contains_ok;
            let mut why = Vec::new();
            if !exit_ok {
                why.push(format!(
                    "expected exit {:?}, got {:?}",
                    clause.expect_exit, code
                ));
            }
            if !contains_ok {
                why.push("missing expected output".into());
            }
            let verdict = if passed {
                ClauseVerdict::Pass
            } else {
                ClauseVerdict::Fail
            };
            let evidence = format!(
                "$ {cmd}\n[exit {}] {}\n{}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                why.join("; "),
                truncate(&combined, 300)
            );
            (verdict, evidence)
        }
        Err(e) => (
            ClauseVerdict::Unverifiable,
            format!("$ {cmd}\nfailed to run: {e}"),
        ),
    };

    ClauseResult {
        kind: "check".into(),
        desc: clause.desc.clone(),
        verdict,
        evidence,
    }
}

/// Run a `review` clause as a **panel** of N independent, adversarial reviewers
/// (in parallel), passing only on a strict majority of "met" votes. Each
/// reviewer's vote + reasoning is recorded, so the human reviews the panel.
/// The implementation lives with fan-out's `JudgePanel` reducer so delegation
/// clauses and fan-out reduction share one reviewer code path.
pub async fn run_review_panel(
    reviewers: &[Arc<dyn Provider>],
    ctx: &Ctx,
    contract: &Contract,
    clause: &DoneClause,
    work_summary: &str,
    reviewed_diff: Option<&ReviewedDiff>,
) -> ClauseResult {
    crate::fan_out::run_judge_panel(
        reviewers,
        ctx,
        contract,
        clause,
        work_summary,
        reviewed_diff,
    )
    .await
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

/// A human-readable verdict bundle — what you read instead of the transcript.
pub fn render_bundle(v: &Verdict, head_hash_hex: &str) -> String {
    let checks = v.results.iter().filter(|r| r.kind == "check").count();
    let reviews = v.results.iter().filter(|r| r.kind == "review").count();
    let mut out = String::new();
    out.push_str(&format!("CONTRACT {} — {}\n", v.contract_id, v.title));
    out.push_str(&format!(
        "VERDICT: {:?}  ({checks} checks, {reviews} reviews)\n\n",
        v.overall
    ));
    for r in &v.results {
        let first_line = r.evidence.lines().next().unwrap_or("");
        out.push_str(&format!(
            "  {} {:<6} {}\n        → {}\n",
            r.verdict.glyph(),
            r.kind,
            r.desc,
            first_line
        ));
    }
    out.push_str(&format!(
        "\n  budget: ${:.4} · {} steps · {} tokens\n",
        v.usage.cost_usd,
        v.usage.steps,
        v.usage.total_tokens()
    ));
    if let Some(diff) = &v.reviewed_diff {
        out.push_str(&format!(
            "  reviewed diff: {} bytes sha256={}{} ({})\n",
            diff.reviewed_bytes,
            &diff.reviewed_sha256[..16.min(diff.reviewed_sha256.len())],
            if diff.truncated { " truncated" } else { "" },
            diff.source
        ));
    }
    out.push_str(&format!(
        "  ledger: head {}… (signed; `--verify {}` to check)\n",
        &head_hash_hex[..16.min(head_hash_hex.len())],
        v.contract_id
    ));
    out
}

struct DiffBaseline {
    kind: DiffBaselineKind,
}

enum DiffBaselineKind {
    Git(Vec<GitRepoBaseline>),
    Path(Vec<PathTreeBaseline>),
}

struct GitRepoBaseline {
    repo: PathBuf,
    before_tree: String,
}

struct PathTreeBaseline {
    root: PathBuf,
    files: BTreeMap<String, FileSnapshot>,
}

#[derive(Clone, PartialEq, Eq)]
struct FileSnapshot {
    bytes: Vec<u8>,
}

const MAX_SNAPSHOT_FILES: usize = 512;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 1_000_000;

impl DiffBaseline {
    fn capture(self) -> Result<ReviewedDiff, String> {
        match self.kind {
            DiffBaselineKind::Git(repos) => Self::capture_git(repos),
            DiffBaselineKind::Path(paths) => Self::capture_paths(paths),
        }
    }

    fn capture_git(repos: Vec<GitRepoBaseline>) -> Result<ReviewedDiff, String> {
        let mut raw = String::new();
        let mut sources = Vec::new();
        for baseline in repos {
            sources.push(baseline.repo.display().to_string());
            let after_tree = write_worktree_tree(&baseline.repo)?;
            raw.push_str(
                &git_output(
                    &baseline.repo,
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--binary",
                        "--find-renames",
                        "--find-copies",
                        &baseline.before_tree,
                        &after_tree,
                        "--",
                    ],
                )
                .map_err(|e| format!("git diff failed in {}: {e}", baseline.repo.display()))?,
            );
        }

        Ok(bound_reviewed_diff(
            format!(
                "git working-tree baseline comparison: {}",
                sources.join(", ")
            ),
            raw,
            MAX_REVIEW_DIFF_BYTES,
        ))
    }

    fn capture_paths(paths: Vec<PathTreeBaseline>) -> Result<ReviewedDiff, String> {
        let mut raw = String::new();
        let mut sources = Vec::new();
        for baseline in paths {
            sources.push(baseline.root.display().to_string());
            let after = snapshot_root(&baseline.root)?;
            raw.push_str(&synthesize_path_diff(
                &baseline.root,
                &baseline.files,
                &after,
            ));
        }

        Ok(bound_reviewed_diff(
            format!("filesystem scope snapshot: {}", sources.join(", ")),
            raw,
            MAX_REVIEW_DIFF_BYTES,
        ))
    }
}

fn capture_diff_baseline(contract: &Contract) -> Result<DiffBaseline, String> {
    let roots = diff_roots(contract).map_err(|e| e.to_string())?;
    let mut repos = BTreeSet::new();
    for root in roots {
        if let Some(repo) = git_root(&root) {
            repos.insert(repo);
        }
    }
    if !repos.is_empty() {
        let repos = repos
            .into_iter()
            .map(|repo| {
                let before_tree = write_worktree_tree(&repo)?;
                Ok(GitRepoBaseline { repo, before_tree })
            })
            .collect::<Result<Vec<_>, String>>()?;
        return Ok(DiffBaseline {
            kind: DiffBaselineKind::Git(repos),
        });
    }

    let Some(write_scope) = &contract.constraints.write_scope else {
        return Err("no git working copy or write scope found for contract scope".into());
    };
    if write_scope.is_empty() {
        return Err("no git working copy or write scope found for contract scope".into());
    }
    let paths = write_scope
        .iter()
        .map(|root| {
            Ok(PathTreeBaseline {
                root: root.clone(),
                files: snapshot_root(root)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(DiffBaseline {
        kind: DiffBaselineKind::Path(paths),
    })
}

fn diff_roots(contract: &Contract) -> Result<Vec<PathBuf>, std::io::Error> {
    if let Some(write_scope) = &contract.constraints.write_scope {
        if !write_scope.is_empty() {
            return Ok(write_scope.clone());
        }
    }
    if let Some(read_scope) = &contract.constraints.read_scope {
        if !read_scope.is_empty() {
            return Ok(read_scope.clone());
        }
    }
    std::env::current_dir().map(|cwd| vec![cwd])
}

fn git_root(path: &Path) -> Option<PathBuf> {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn write_worktree_tree(repo: &Path) -> Result<String, String> {
    let index = temp_index_path();
    let result = (|| {
        if has_head(repo)? {
            git_status(repo, &["read-tree", "HEAD"], Some(&index))?;
        } else {
            git_status(repo, &["read-tree", "--empty"], Some(&index))?;
        }
        git_status(repo, &["add", "-A", "--", "."], Some(&index))?;
        git_output_with_index(repo, &["write-tree"], &index).map(|tree| tree.trim().to_string())
    })();
    let _ = std::fs::remove_file(index);
    result
}

fn has_head(repo: &Path) -> Result<bool, String> {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "HEAD^{tree}"])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(output.status.success())
}

fn git_status(repo: &Path, args: &[&str], index: Option<&Path>) -> Result<(), String> {
    let mut command = StdCommand::new("git");
    command.arg("-C").arg(repo).args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn git_output_with_index(repo: &Path, args: &[&str], index: &Path) -> Result<String, String> {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn temp_index_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "truss-diff-index-{}-{nanos:x}.idx",
        std::process::id()
    ))
}

fn snapshot_root(root: &Path) -> Result<BTreeMap<String, FileSnapshot>, String> {
    let mut files = BTreeMap::new();
    if !root.exists() {
        return Ok(files);
    }
    collect_snapshots(root, root, &mut files)?;
    Ok(files)
}

fn collect_snapshots(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, FileSnapshot>,
) -> Result<(), String> {
    if files.len() >= MAX_SNAPSHOT_FILES {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            collect_snapshots(root, &entry.path(), files)?;
            if files.len() >= MAX_SNAPSHOT_FILES {
                break;
            }
        }
        return Ok(());
    }
    if !meta.is_file() || meta.len() > MAX_SNAPSHOT_FILE_BYTES {
        return Ok(());
    }
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    files.insert(rel, FileSnapshot { bytes });
    Ok(())
}

fn synthesize_path_diff(
    root: &Path,
    before: &BTreeMap<String, FileSnapshot>,
    after: &BTreeMap<String, FileSnapshot>,
) -> String {
    let mut rels = before.keys().chain(after.keys()).collect::<BTreeSet<_>>();
    let mut out = String::new();
    for rel in std::mem::take(&mut rels) {
        let old = before.get(rel);
        let new = after.get(rel);
        if old == new {
            continue;
        }
        out.push_str(&format!(
            "diff --truss a/{rel} b/{rel}\n# scope root: {}\n",
            root.display()
        ));
        match (old, new) {
            (None, Some(new)) => {
                out.push_str("--- /dev/null\n");
                out.push_str(&format!("+++ b/{rel}\n"));
                out.push_str(&prefixed_file(&new.bytes, '+'));
            }
            (Some(old), None) => {
                out.push_str(&format!("--- a/{rel}\n"));
                out.push_str("+++ /dev/null\n");
                out.push_str(&prefixed_file(&old.bytes, '-'));
            }
            (Some(old), Some(new)) => {
                out.push_str(&format!("--- a/{rel}\n"));
                out.push_str(&format!("+++ b/{rel}\n"));
                out.push_str(&prefixed_file(&old.bytes, '-'));
                out.push_str(&prefixed_file(&new.bytes, '+'));
            }
            (None, None) => {}
        }
    }
    out
}

fn prefixed_file(bytes: &[u8], prefix: char) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text
            .lines()
            .map(|line| format!("{prefix}{line}\n"))
            .collect::<String>(),
        Err(_) => format!(
            "{prefix}[binary file: {} bytes sha256={}]\n",
            bytes.len(),
            sha256_hex(bytes)
        ),
    }
}

fn bound_reviewed_diff(source: String, raw: String, max_bytes: usize) -> ReviewedDiff {
    let full_bytes = raw.len();
    let full_sha256 = sha256_hex(raw.as_bytes());
    let (mut content, truncated) = if raw.len() <= max_bytes {
        (raw, false)
    } else {
        let mut end = max_bytes;
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        (raw[..end].to_string(), true)
    };
    if truncated {
        content.push_str(&format!(
            "\n[diff truncated: showing {} of {full_bytes} bytes; full sha256={full_sha256}]\n",
            content.len()
        ));
    }
    let reviewed_bytes = content.len();
    let reviewed_sha256 = sha256_hex(content.as_bytes());
    ReviewedDiff {
        source,
        full_sha256,
        reviewed_sha256,
        full_bytes,
        reviewed_bytes,
        truncated,
        content,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn other(e: anyhow::Error) -> HarnessError {
    HarnessError::Other(e.to_string())
}

fn memory_other(e: crate::memory::MemoryError) -> HarnessError {
    HarnessError::Other(e.to_string())
}
