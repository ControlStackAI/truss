use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{Barrier, Notify};

use truss_core::budget::{BudgetLimits, BudgetMeter};
use truss_core::child_run::ChildRunOutput;
use truss_core::ctx::Ctx;
use truss_core::delegation::{ClauseResult, ClauseVerdict, Verdict};
use truss_core::error::{HarnessError, ProviderError};
use truss_core::events::{AgentEvent, FanOutReducerKind};
use truss_core::fan_out::{run_fan_out, BestScore, FanOutOptions, Reducer};
use truss_core::message::Block;
use truss_core::provider::{
    CompletionRequest, CompletionResponse, ModelCapabilities, Provider, StopReason, Usage,
};
use truss_core::store::{Store, CHILD_ANCHOR_KIND, FANOUT_REDUCTION_KIND};

fn verdict(id: &str, overall: ClauseVerdict, summary: &str) -> Verdict {
    Verdict {
        contract_id: id.into(),
        title: "fan-out child".into(),
        overall: overall.clone(),
        results: vec![ClauseResult {
            kind: "check".into(),
            desc: "child result is verified".into(),
            verdict: overall,
            evidence: summary.into(),
        }],
        executor_summary: summary.into(),
        reviewed_diff: None,
        usage: Default::default(),
    }
}

fn options(k: usize, reducer: Reducer) -> FanOutOptions {
    FanOutOptions::new(
        "solve the same task",
        k,
        BudgetLimits {
            max_steps: Some(10),
            ..Default::default()
        },
    )
    .with_scope("repo:truss")
    .with_reducer(reducer)
}

async fn scripted_fanout(
    reducer: Reducer,
    verdicts: Vec<ClauseVerdict>,
    summaries: Vec<&'static str>,
) -> truss_core::fan_out::FanOutResult {
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let verdicts = Arc::new(verdicts);
    let summaries = Arc::new(summaries);
    run_fan_out(&ctx, &store, options(verdicts.len(), reducer), {
        let verdicts = verdicts.clone();
        let summaries = summaries.clone();
        move |branch, child_ctx, _store| {
            let verdicts = verdicts.clone();
            let summaries = summaries.clone();
            async move {
                child_ctx.budget.record_step();
                Ok(ChildRunOutput {
                    verdict: verdict(
                        &format!("child-{}", branch.index),
                        verdicts[branch.index].clone(),
                        summaries[branch.index],
                    ),
                    artifacts: Vec::new(),
                })
            }
        }
    })
    .await
    .unwrap()
}

struct SummaryScore;

impl BestScore for SummaryScore {
    fn score(&self, result: &truss_core::child_run::VerifiedChildResult) -> f64 {
        result
            .verdict
            .executor_summary
            .strip_prefix("score=")
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0)
    }
}

struct JudgeProvider;

#[async_trait]
impl Provider for JudgeProvider {
    fn name(&self) -> &str {
        "judge"
    }

    fn default_model(&self) -> &str {
        "judge-model"
    }

    fn capabilities(&self, _model: Option<&str>) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let prompt = req
            .messages
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>()
            .join("\n");
        let met = prompt.contains("approved-by-panel");
        Ok(CompletionResponse {
            content: vec![Block::Text(
                json!({ "met": met, "reasoning": if met { "best evidence" } else { "not enough" } })
                    .to_string(),
            )],
            stop_reason: StopReason::EndTurn,
            model: "judge-model".into(),
            usage: Usage::default(),
        })
    }
}

#[tokio::test]
async fn reducers_select_expected_winners() {
    let vote = scripted_fanout(
        Reducer::Vote,
        vec![
            ClauseVerdict::Fail,
            ClauseVerdict::Pass,
            ClauseVerdict::Pass,
        ],
        vec!["bad", "good-a", "good-b"],
    )
    .await;
    assert_eq!(vote.reducer, FanOutReducerKind::Vote);
    assert_eq!(vote.winner.index, 1);

    let best = scripted_fanout(
        Reducer::BestScore {
            scorer: Arc::new(SummaryScore),
        },
        vec![
            ClauseVerdict::Pass,
            ClauseVerdict::Pass,
            ClauseVerdict::Pass,
        ],
        vec!["score=2", "score=9", "score=5"],
    )
    .await;
    assert_eq!(best.reducer, FanOutReducerKind::BestScore);
    assert_eq!(best.winner.index, 1);

    let first = scripted_fanout(
        Reducer::FirstSuccess,
        vec![ClauseVerdict::Fail, ClauseVerdict::Pass],
        vec!["not it", "first pass"],
    )
    .await;
    assert_eq!(first.reducer, FanOutReducerKind::FirstSuccess);
    assert_eq!(first.winner.index, 1);

    let panel = scripted_fanout(
        Reducer::JudgePanel {
            reviewers: vec![Arc::new(JudgeProvider)],
            criterion: "Accept only the best verified branch.".into(),
        },
        vec![ClauseVerdict::Pass, ClauseVerdict::Pass],
        vec!["ordinary", "approved-by-panel"],
    )
    .await;
    assert_eq!(panel.reducer, FanOutReducerKind::JudgePanel);
    assert_eq!(panel.winner.index, 1);
}

#[tokio::test]
async fn fanout_records_child_anchors_reduction_decision_events_and_verifies() {
    let store = Store::open_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);

    let result = run_fan_out(
        &ctx,
        &store,
        options(3, Reducer::Vote),
        |branch, child_ctx, _store| async move {
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: verdict(
                    &format!("child-{}", branch.index),
                    ClauseVerdict::Pass,
                    "verified",
                ),
                artifacts: Vec::new(),
            })
        },
    )
    .await
    .unwrap();

    let entries = store.entries(&ctx.run_id).unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind == CHILD_ANCHOR_KIND)
            .count(),
        3
    );
    let reduction = entries
        .iter()
        .find(|entry| entry.kind == FANOUT_REDUCTION_KIND)
        .expect("fanout_reduction entry");
    assert_eq!(reduction.seq, result.decision_ref.seq);
    assert!(store.verify_chain(&ctx.run_id).unwrap().is_ok());

    let mut saw_started = false;
    let mut saw_completed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::FanoutStarted {
                fanout_id,
                k,
                reducer,
                ..
            } => {
                saw_started =
                    fanout_id == result.fanout_id && k == 3 && reducer == FanOutReducerKind::Vote;
            }
            AgentEvent::FanoutCompleted {
                fanout_id,
                child_results,
                winner,
                decision_ref,
                ..
            } => {
                saw_completed = fanout_id == result.fanout_id
                    && child_results.len() == 3
                    && winner.as_ref().map(|winner| winner.index) == Some(result.winner.index)
                    && decision_ref == result.decision_ref;
            }
            _ => {}
        }
    }
    assert!(saw_started, "fanout_started event emitted");
    assert!(saw_completed, "fanout_completed event emitted");

    store
        .raw_set_payload_for_test(
            &ctx.run_id,
            result.decision_ref.seq,
            &json!({
                "parent_run_id": ctx.run_id,
                "fanout_id": result.fanout_id,
                "reducer": "vote",
                "children": result.child_results,
                "winner": {
                    "index": result.winner.index,
                    "child_run_id": "not-a-child",
                    "verdict_ref": result.winner.verdict_ref,
                },
                "why": result.why,
            }),
        )
        .unwrap();
    assert!(
        store.verify_chain(&ctx.run_id).unwrap().is_err(),
        "fanout_reduction refs are verified"
    );
}

#[tokio::test]
async fn k_branches_cannot_exceed_parent_step_ceiling() {
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new().with_budget(BudgetMeter::new(BudgetLimits {
        max_steps: Some(4),
        ..Default::default()
    }));
    let before_record = Arc::new(Barrier::new(5));
    let after_record = Arc::new(Barrier::new(5));

    let err = run_fan_out(&ctx, &store, options(5, Reducer::Vote), {
        let before_record = before_record.clone();
        let after_record = after_record.clone();
        move |_branch, child_ctx, _store| {
            let before_record = before_record.clone();
            let after_record = after_record.clone();
            async move {
                before_record.wait().await;
                child_ctx.budget.record_step();
                after_record.wait().await;
                Ok(ChildRunOutput {
                    verdict: verdict("budget", ClauseVerdict::Pass, "budget"),
                    artifacts: Vec::new(),
                })
            }
        }
    })
    .await
    .unwrap_err();

    assert!(
        matches!(err, HarnessError::BudgetExceeded(_)),
        "got {err:?}"
    );
    assert_eq!(ctx.budget.snapshot().steps, 4);
}

#[tokio::test]
async fn first_success_cancels_losing_branches() {
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let all_started = Arc::new(Barrier::new(3));
    let losers_cancelled = Arc::new(AtomicUsize::new(0));

    let result = run_fan_out(&ctx, &store, options(3, Reducer::FirstSuccess), {
        let all_started = all_started.clone();
        let losers_cancelled = losers_cancelled.clone();
        move |branch, child_ctx, _store| {
            let all_started = all_started.clone();
            let losers_cancelled = losers_cancelled.clone();
            async move {
                all_started.wait().await;
                if branch.index == 0 {
                    child_ctx.budget.record_step();
                    return Ok(ChildRunOutput {
                        verdict: verdict("winner", ClauseVerdict::Pass, "winner"),
                        artifacts: Vec::new(),
                    });
                }
                child_ctx.cancel_token().cancelled().await;
                losers_cancelled.fetch_add(1, Ordering::SeqCst);
                Err(HarnessError::Cancelled)
            }
        }
    })
    .await
    .unwrap();

    assert_eq!(result.winner.index, 0);
    assert_eq!(losers_cancelled.load(Ordering::SeqCst), 2);
    assert_eq!(result.child_results.len(), 3);
    assert_eq!(
        result
            .child_results
            .iter()
            .filter(|child| child.status == "failed")
            .count(),
        2
    );
}

#[tokio::test]
async fn parent_cancel_propagates_to_all_branches() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new();
    let started = Arc::new(AtomicUsize::new(0));
    let cancelled = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let parent_for_task = parent.clone();
    let store_for_task = store.clone();

    let task = tokio::spawn({
        let started = started.clone();
        let cancelled = cancelled.clone();
        let notify = notify.clone();
        async move {
            run_fan_out(
                &parent_for_task,
                &store_for_task,
                options(3, Reducer::Vote),
                move |_branch, child_ctx, _store| {
                    let started = started.clone();
                    let cancelled = cancelled.clone();
                    let notify = notify.clone();
                    async move {
                        if started.fetch_add(1, Ordering::SeqCst) == 2 {
                            notify.notify_one();
                        }
                        child_ctx.cancel_token().cancelled().await;
                        cancelled.fetch_add(1, Ordering::SeqCst);
                        Err(HarnessError::Cancelled)
                    }
                },
            )
            .await
        }
    });

    notify.notified().await;
    parent.cancel();
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, HarnessError::Cancelled), "got {err:?}");
    assert_eq!(cancelled.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn parent_deadline_propagates_to_all_branches() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new().with_timeout(Duration::from_millis(10));
    let started = Arc::new(AtomicUsize::new(0));
    let expired = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    let task = tokio::spawn({
        let parent = parent.clone();
        let store = store.clone();
        let started = started.clone();
        let expired = expired.clone();
        let notify = notify.clone();
        async move {
            run_fan_out(
                &parent,
                &store,
                options(3, Reducer::Vote),
                move |_branch, child_ctx, _store| {
                    let started = started.clone();
                    let expired = expired.clone();
                    let notify = notify.clone();
                    async move {
                        if started.fetch_add(1, Ordering::SeqCst) == 2 {
                            notify.notify_one();
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        if child_ctx.check().is_err() {
                            expired.fetch_add(1, Ordering::SeqCst);
                        }
                        child_ctx.check()?;
                        Ok(ChildRunOutput {
                            verdict: verdict("deadline", ClauseVerdict::Pass, "deadline"),
                            artifacts: Vec::new(),
                        })
                    }
                },
            )
            .await
        }
    });

    notify.notified().await;
    tokio::time::advance(Duration::from_millis(20)).await;
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, HarnessError::Cancelled), "got {err:?}");
    assert_eq!(expired.load(Ordering::SeqCst), 3);
}
