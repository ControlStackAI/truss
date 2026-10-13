use std::time::Duration;

use truss_core::budget::{BudgetLimits, BudgetMeter};
use truss_core::child_run::{run_child, ChildRunOptions, ChildRunOutput};
use truss_core::ctx::Ctx;
use truss_core::delegation::{ClauseResult, ClauseVerdict, Verdict};
use truss_core::error::HarnessError;
use truss_core::events::{AgentEvent, ArtifactRef};
use truss_core::store::{Store, CHILD_ANCHOR_KIND};

fn verdict(id: &str) -> Verdict {
    Verdict {
        contract_id: id.into(),
        title: "child task".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "check".into(),
            desc: "child result is verified".into(),
            verdict: ClauseVerdict::Pass,
            evidence: "fake-provider evidence".into(),
        }],
        executor_summary: "child produced the artifact".into(),
        reviewed_diff: None,
        usage: Default::default(),
    }
}

fn artifact() -> ArtifactRef {
    ArtifactRef {
        path: "out/report.txt".into(),
        kind: "text/plain".into(),
        sha256: "abc123".into(),
        size_bytes: 12,
    }
}

fn options(max_steps: u64) -> ChildRunOptions {
    ChildRunOptions::new(
        "produce a verified artifact",
        BudgetLimits {
            max_steps: Some(max_steps),
            ..Default::default()
        },
    )
    .with_scope("repo:truss")
}

#[tokio::test]
async fn child_returns_signed_verified_result_and_events() {
    let store = Store::open_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let parent = Ctx::new().with_events(tx);

    let result = run_child(
        &parent,
        &store,
        options(5),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: verdict("child-a"),
                artifacts: vec![artifact()],
            })
        },
    )
    .await
    .unwrap();

    assert_eq!(result.verdict.overall, ClauseVerdict::Pass);
    assert_eq!(result.artifacts, vec![artifact()]);
    assert_eq!(result.usage.steps, 1);
    assert!(store.verify_chain(&result.child_run_id).unwrap().is_ok());

    let child_entries = store.entries(&result.child_run_id).unwrap();
    let signed_result = child_entries
        .iter()
        .find(|entry| entry.kind == "child_result")
        .expect("child_result entry");
    assert!(!signed_result.signature.is_empty());
    assert_eq!(result.verdict_ref.seq, signed_result.seq);

    let mut saw_started = false;
    let mut saw_completed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ChildStarted { child_run_id, .. } => {
                saw_started = child_run_id == result.child_run_id;
            }
            AgentEvent::ChildCompleted {
                child_run_id,
                child_head_hash,
                artifacts,
                ..
            } => {
                saw_completed = child_run_id == result.child_run_id
                    && child_head_hash == result.child_head_hash
                    && artifacts == vec![artifact()];
            }
            _ => {}
        }
    }
    assert!(saw_started, "child_started event emitted");
    assert!(saw_completed, "child_completed event emitted");
}

#[tokio::test]
async fn parent_anchor_verification_recurses_into_child_ledger() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new();

    let result = run_child(
        &parent,
        &store,
        options(5),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: verdict("child-anchor"),
                artifacts: vec![artifact()],
            })
        },
    )
    .await
    .unwrap();

    let parent_entries = store.entries(&parent.run_id).unwrap();
    let anchor = parent_entries
        .iter()
        .find(|entry| entry.kind == CHILD_ANCHOR_KIND)
        .expect("parent child_anchor entry");
    assert_eq!(anchor.seq, result.parent_anchor.seq);
    assert_eq!(
        anchor
            .payload
            .get("child_head_hash")
            .and_then(|value| value.as_str()),
        Some(result.child_head_hash.as_str())
    );
    assert!(store.verify_chain(&parent.run_id).unwrap().is_ok());

    store
        .raw_set_payload_for_test(
            &result.child_run_id,
            result.verdict_ref.seq,
            &serde_json::json!({ "tampered": true }),
        )
        .unwrap();
    assert!(
        store.verify_chain(&parent.run_id).unwrap().is_err(),
        "parent verification fails when an anchored child is tampered"
    );
}

#[tokio::test]
async fn child_slice_is_enforced_and_charged_to_parent_tree_budget() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new().with_budget(BudgetMeter::new(BudgetLimits {
        max_steps: Some(4),
        ..Default::default()
    }));

    let slice_err = run_child(
        &parent,
        &store,
        options(2),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: verdict("slice"),
                artifacts: Vec::new(),
            })
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(slice_err, HarnessError::BudgetExceeded(_)),
        "slice should reject the child at its own ceiling: {slice_err:?}"
    );

    let parent = Ctx::new().with_budget(BudgetMeter::new(BudgetLimits {
        max_steps: Some(4),
        ..Default::default()
    }));
    run_child(
        &parent,
        &store,
        options(10),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: verdict("fleet-1"),
                artifacts: Vec::new(),
            })
        },
    )
    .await
    .unwrap();

    let fleet_err = run_child(
        &parent,
        &store,
        options(10),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: verdict("fleet-2"),
                artifacts: Vec::new(),
            })
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(fleet_err, HarnessError::BudgetExceeded(_)),
        "child fleet should share the parent ceiling: {fleet_err:?}"
    );
    assert_eq!(parent.budget.snapshot().steps, 4);
}

#[tokio::test]
async fn parent_cancel_propagates_to_running_child() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let parent_for_task = parent.clone();
    let store_for_task = store.clone();

    let task = tokio::spawn(async move {
        run_child(
            &parent_for_task,
            &store_for_task,
            options(5),
            |child_ctx, _store| async move {
                started_tx.send(()).unwrap();
                child_ctx.cancel_token().cancelled().await;
                Err(HarnessError::Cancelled)
            },
        )
        .await
    });

    started_rx.await.unwrap();
    parent.cancel();
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, HarnessError::Cancelled));
}

#[tokio::test(start_paused = true)]
async fn parent_deadline_is_inherited_by_child() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new().with_timeout(Duration::from_millis(10));

    let err = run_child(
        &parent,
        &store,
        options(5),
        |child_ctx, _store| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            child_ctx.check()?;
            Ok(ChildRunOutput {
                verdict: verdict("deadline"),
                artifacts: Vec::new(),
            })
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, HarnessError::Cancelled));
}
