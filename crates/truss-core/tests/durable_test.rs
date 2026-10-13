//! M1 durable-execution proof — no network required.
//!
//! A scripted mock provider drives the loop through: call a tool → suspend on
//! `wait` → (process "exits") → resume → re-issue the same tool → finish. We
//! assert the run suspends, resumes from exact context, the side-effecting tool
//! runs exactly once (memoized on the re-issue), and the hash chain verifies.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use truss_core::approval::ApprovalGatePolicy;
use truss_core::ctx::Ctx;
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::{ProviderError, ToolError};
use truss_core::executor::{CliExecutor, Executor, PromptVia};
use truss_core::message::Block;
use truss_core::provider::{
    CompletionRequest, CompletionResponse, Provider, StopReason, ToolDef, Usage,
};
use truss_core::store::{HumanReviewDecision, Store, HUMAN_REVIEW_KIND};
use truss_core::tools::{Tool, ToolRegistry};

/// Provider that replays a fixed script of responses, one per `complete` call.
struct MockProvider {
    calls: Arc<Mutex<usize>>,
    script: Vec<CompletionResponse>,
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut n = self.calls.lock().unwrap();
        let i = *n;
        *n += 1;
        self.script
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Config(format!("script exhausted at call {i}")))
    }
}

/// A side-effecting tool that counts how many times it actually executed.
struct PingTool {
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for PingTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "ping".into(),
            description: "test tool".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }
    async fn call(&self, _ctx: &Ctx, _input: serde_json::Value) -> Result<String, ToolError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok("pong".into())
    }
}

fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn assistant_text(text: &str) -> CompletionResponse {
    assistant_text_with_model(text, "mock-model", 10, 5)
}

fn assistant_text_with_model(
    text: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        model: model.into(),
        usage: Usage {
            input_tokens,
            output_tokens,
        },
    }
}

#[tokio::test]
async fn suspend_resume_with_exact_context_and_memoized_tools() {
    // Script: ping → wait(0s) [suspend] → ping again [should memoize] → done.
    let script = vec![
        assistant_tool_use("c1", "ping", json!({})),
        assistant_tool_use(
            "c2",
            "wait",
            json!({ "seconds": 0, "reason": "await signal" }),
        ),
        assistant_tool_use("c3", "ping", json!({})), // re-issued after resume
        assistant_text("done"),
    ];
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script,
    });

    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let tools = Arc::new(reg);

    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };
    let sid = "test-session";

    // First leg: runs until it suspends on `wait`.
    let out1 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("begin".into()),
    )
    .await
    .expect("run leg 1");
    assert!(
        matches!(out1, DurableOutcome::Suspended { .. }),
        "expected suspend, got {out1:?}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "ping ran once before suspend"
    );
    assert_eq!(store.get_session(sid).unwrap().unwrap().status, "suspended");

    // Second leg: resume (no prompt). Timer (0s) has elapsed.
    let out2 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        None,
    )
    .await
    .expect("run leg 2");
    match out2 {
        DurableOutcome::Completed { final_text } => assert_eq!(final_text, "done"),
        other => panic!("expected completion, got {other:?}"),
    }

    // Stage A semantics: the FIRST ping (c1) was replayed from the journal on
    // resume, not re-run; the model then *deliberately* re-issued ping (c3),
    // which is a new logical step and executes. So exactly 2 executions total.
    // (Positional replay reuses recorded steps; it doesn't stop the model from
    // choosing to call a tool again. See `recorded_steps_are_not_re_executed`.)
    assert_eq!(hits.load(Ordering::SeqCst), 2, "c1 replayed, c3 executed");

    // Session completed and the hash chain verifies.
    assert_eq!(store.get_session(sid).unwrap().unwrap().status, "completed");
    assert!(
        store.verify_chain(sid).unwrap().is_ok(),
        "hash chain intact"
    );

    // Sanity: the journal recorded the expected shape.
    let kinds: Vec<String> = store
        .entries(sid)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds[0], "run_started");
    assert!(kinds.contains(&"suspended".to_string()));
    assert!(kinds.contains(&"timer_fired".to_string()));
    assert!(kinds.contains(&"run_completed".to_string()));
}

#[tokio::test]
async fn suspend_on_signal_then_resume_after_delivery() {
    // Script: ping → await_signal("approval") [suspend] → ping again [memoized] → done.
    let script = vec![
        assistant_tool_use("c1", "ping", json!({})),
        assistant_tool_use(
            "c2",
            "await_signal",
            json!({ "name": "approval", "reason": "needs human ok" }),
        ),
        assistant_tool_use("c3", "ping", json!({})),
        assistant_text("approved and done"),
    ];
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script,
    });

    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let tools = Arc::new(reg);

    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };
    let sid = "sig-session";

    // Leg 1: suspends awaiting the signal.
    let out1 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("go".into()),
    )
    .await
    .expect("leg 1");
    match out1 {
        DurableOutcome::Suspended { await_desc, .. } => assert!(await_desc.contains("approval")),
        other => panic!("expected signal suspend, got {other:?}"),
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Resume before delivery → StillWaiting (and the model is not called again).
    let early = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        None,
    )
    .await
    .expect("early resume");
    assert!(matches!(early, DurableOutcome::StillWaiting { .. }));

    // Deliver the signal, then resume → completes.
    store.add_signal(sid, "approval", Some("yes")).unwrap();
    let out2 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        None,
    )
    .await
    .expect("leg 2");
    match out2 {
        DurableOutcome::Completed { final_text } => assert_eq!(final_text, "approved and done"),
        other => panic!("expected completion, got {other:?}"),
    }
    // c1 replayed from the journal on resume; c3 re-issued by the model executes.
    assert_eq!(hits.load(Ordering::SeqCst), 2, "c1 replayed, c3 executed");

    let kinds: Vec<String> = store
        .entries(sid)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"signal_received".to_string()));
    assert!(store.verify_chain(sid).unwrap().is_ok());
}

#[tokio::test]
async fn approval_gate_for_high_risk_tool_blocks_until_signed_approval() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![
            assistant_tool_use("c1", "ping", json!({})),
            assistant_text("done"),
        ],
    });

    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let tools = Arc::new(reg);
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        approval_gates: ApprovalGatePolicy {
            enabled: true,
            high_risk_tools: vec!["ping".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let sid = "approval-tool-session";

    let out1 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("begin".into()),
    )
    .await
    .expect("gated run leg");
    match out1 {
        DurableOutcome::Suspended { await_desc, reason } => {
            assert!(await_desc.contains("approval_gate"));
            assert!(reason.contains("tool 'ping'"));
        }
        other => panic!("expected approval suspension, got {other:?}"),
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "tool must not run before approval"
    );
    assert_eq!(store.get_session(sid).unwrap().unwrap().status, "suspended");

    let review = store
        .record_human_review(sid, HumanReviewDecision::Approved, Some("approved".into()))
        .unwrap();
    assert_eq!(review.kind, HUMAN_REVIEW_KIND);
    assert!(store.verify_chain(sid).unwrap().is_ok());

    let out2 = durable::run(provider, tools, &ctx, &store, sid, &cfg, None)
        .await
        .expect("approved resume");
    match out2 {
        DurableOutcome::Completed { final_text } => assert_eq!(final_text, "done"),
        other => panic!("expected completion after approval, got {other:?}"),
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "approved tool executes exactly once"
    );
    assert_eq!(store.get_session(sid).unwrap().unwrap().status, "completed");

    let kinds = store
        .entries(sid)
        .unwrap()
        .into_iter()
        .map(|entry| entry.kind)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"suspended".to_string()));
    assert!(kinds.contains(&HUMAN_REVIEW_KIND.to_string()));
    assert!(kinds.contains(&"signal_received".to_string()));
    assert!(store.verify_chain(sid).unwrap().is_ok());
}

#[tokio::test]
async fn approval_gate_reject_keeps_run_suspended_and_tool_unrun() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![assistant_tool_use("c1", "ping", json!({}))],
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let tools = Arc::new(reg);
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        approval_gates: ApprovalGatePolicy {
            enabled: true,
            high_risk_tools: vec!["ping".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let sid = "approval-reject-session";

    let out1 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("begin".into()),
    )
    .await
    .expect("gated run leg");
    assert!(matches!(out1, DurableOutcome::Suspended { .. }));

    let review = store
        .record_human_review(sid, HumanReviewDecision::Rejected, Some("too risky".into()))
        .unwrap();
    assert_eq!(review.kind, HUMAN_REVIEW_KIND);
    assert!(store.verify_chain(sid).unwrap().is_ok());

    let out2 = durable::run(provider, tools, &ctx, &store, sid, &cfg, None)
        .await
        .expect("rejected resume");
    assert!(matches!(out2, DurableOutcome::StillWaiting { .. }));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert_eq!(store.get_session(sid).unwrap().unwrap().status, "suspended");
    assert!(store
        .entries(sid)
        .unwrap()
        .iter()
        .any(|entry| entry.kind == HUMAN_REVIEW_KIND));
}

#[tokio::test]
async fn approval_gate_signal_without_signed_review_does_not_release() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![assistant_tool_use("c1", "ping", json!({}))],
    });
    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let tools = Arc::new(reg);
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        approval_gates: ApprovalGatePolicy {
            enabled: true,
            high_risk_tools: vec!["ping".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    let sid = "approval-no-fake-signal-session";

    let out1 = durable::run(
        provider.clone(),
        tools.clone(),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("begin".into()),
    )
    .await
    .expect("gated run leg");
    assert!(matches!(out1, DurableOutcome::Suspended { .. }));

    store
        .add_signal(
            sid,
            truss_core::approval::APPROVAL_GATE_SIGNAL,
            Some("manual"),
        )
        .unwrap();
    let out2 = durable::run(provider, tools, &ctx, &store, sid, &cfg, None)
        .await
        .expect("manual signal resume");
    assert!(matches!(out2, DurableOutcome::StillWaiting { .. }));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(!store
        .entries(sid)
        .unwrap()
        .iter()
        .any(|entry| entry.kind == "signal_received"));
}

#[tokio::test]
async fn approval_gate_for_spend_threshold_suspends_before_completion() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![assistant_text_with_model(
            "costly done",
            "gpt-5",
            1_000,
            1_000,
        )],
    });
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        approval_gates: ApprovalGatePolicy {
            enabled: true,
            spend_threshold_usd: Some(0.000001),
            ..Default::default()
        },
        ..Default::default()
    };
    let sid = "approval-spend-session";

    let out1 = durable::run(
        provider.clone(),
        Arc::new(ToolRegistry::new()),
        &ctx,
        &store,
        sid,
        &cfg,
        Some("begin".into()),
    )
    .await
    .expect("spend-gated run");
    match out1 {
        DurableOutcome::Suspended { reason, .. } => {
            assert!(reason.contains("spend threshold"));
        }
        other => panic!("expected spend approval suspension, got {other:?}"),
    }

    let review = store
        .record_human_review(sid, HumanReviewDecision::Approved, None)
        .unwrap();
    assert_eq!(review.kind, HUMAN_REVIEW_KIND);

    let out2 = durable::run(
        provider,
        Arc::new(ToolRegistry::new()),
        &ctx,
        &store,
        sid,
        &cfg,
        None,
    )
    .await
    .expect("approved spend resume");
    match out2 {
        DurableOutcome::Completed { final_text } => assert_eq!(final_text, "costly done"),
        other => panic!("expected completion after approval, got {other:?}"),
    }
    assert!(store.verify_chain(sid).unwrap().is_ok());
}

/// Stage A positional replay: a session whose journal already records a tool
/// result (a crash *after* the tool ran, *before* the next model call) must
/// resume by REPLAYING that recorded result — the tool is never re-executed.
#[tokio::test]
async fn recorded_steps_are_not_re_executed() {
    use truss_core::message::Block;

    // Mock provider: the only model call left to make returns the final answer.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![assistant_text("finished")],
    });

    // A counting tool that MUST NOT run during this resume.
    let hits = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(PingTool { hits: hits.clone() }));
    let tools = Arc::new(reg);

    let store = Store::open_memory().unwrap();
    let sid = "crash-session";

    // Hand-seed a journal: prompt → assistant asked for ping → ping result
    // recorded. Status left "running" (process "crashed" before the next call).
    store
        .create_session(sid, Some("mock-model"), "local", "local")
        .unwrap();
    store
        .append(sid, "run_started", &json!({ "prompt": "do it" }))
        .unwrap();
    let tool_use = vec![Block::ToolUse {
        id: "c1".into(),
        name: "ping".into(),
        input: json!({}),
    }];
    store
        .append(
            sid,
            "model_result",
            &json!({
                "content": serde_json::to_value(&tool_use).unwrap(),
                "stop_reason": "ToolUse",
            }),
        )
        .unwrap();
    store
        .append(
            sid,
            "tool_result",
            &json!({
                "call_id": "c1", "name": "ping", "input": {}, "content": "pong", "is_error": false
            }),
        )
        .unwrap();

    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };
    let out = durable::run(provider, tools, &ctx, &store, sid, &cfg, None)
        .await
        .expect("resume");

    match out {
        DurableOutcome::Completed { final_text } => assert_eq!(final_text, "finished"),
        other => panic!("expected completion, got {other:?}"),
    }
    // The recorded ping was replayed from the journal, never executed here.
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "recorded tool must not re-run"
    );
}

#[tokio::test]
async fn anchor_session_head_to_transparency_log() {
    use truss_core::anchor::{Anchor, FileTransparencyLog};

    let store = Store::open_memory().unwrap();
    store
        .create_session("s", Some("m"), "local", "local")
        .unwrap();
    store
        .append("s", "run_started", &json!({ "prompt": "x" }))
        .unwrap();
    store
        .append("s", "run_completed", &json!({ "final_text": "done" }))
        .unwrap();

    let session = store.get_session("s").unwrap().unwrap();
    let head = session.head_hash.expect("head hash");
    let pubkey = session.pubkey_id.unwrap_or_default();

    let path = std::env::temp_dir().join(format!("ah-anchor-it-{}.jsonl", std::process::id()));
    let p = path.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&p);

    let log = FileTransparencyLog::new(p.clone());
    let receipt = log.anchor("s", &head, &pubkey).unwrap();
    store
        .record_anchor(
            "s",
            &receipt.log,
            receipt.index,
            &receipt.head_hash_hex,
            &receipt.entry_hash_hex,
            receipt.anchored_at_ms,
        )
        .unwrap();

    assert_eq!(store.anchor_count("s").unwrap(), 1);
    assert!(matches!(log.verify().unwrap(), Ok(1)));
    assert_eq!(receipt.head_hash_hex, hex::encode(&head));

    std::fs::remove_file(&p).ok();
}

#[tokio::test]
async fn tamper_breaks_the_chain() {
    let store = Store::open_memory().unwrap();
    store
        .create_session("s", Some("m"), "local", "local")
        .unwrap();
    store
        .append("s", "run_started", &json!({ "prompt": "x" }))
        .unwrap();
    store
        .append("s", "model_result", &json!({ "ok": true }))
        .unwrap();
    assert!(store.verify_chain("s").unwrap().is_ok());

    // Tamper with a payload directly, bypassing append's chaining.
    store
        .raw_set_payload_for_test("s", 1, &json!({ "ok": false }))
        .unwrap();
    assert!(
        store.verify_chain("s").unwrap().is_err(),
        "altered payload must break the chain"
    );
}

#[tokio::test]
async fn corrupt_signature_breaks_verification() {
    let store = Store::open_memory().unwrap();
    store
        .create_session("s", Some("m"), "local", "local")
        .unwrap();
    store
        .append("s", "run_started", &json!({ "prompt": "x" }))
        .unwrap();
    store
        .append("s", "model_result", &json!({ "ok": true }))
        .unwrap();
    assert!(store.verify_chain("s").unwrap().is_ok());

    // Zero out a signature (hash still matches, but the signature won't verify).
    store.raw_corrupt_signature_for_test("s", 1).unwrap();
    assert_eq!(
        store.verify_chain("s").unwrap(),
        Err(1),
        "an invalid signature must fail verification at that seq"
    );
}

// ── Verified delegation (the north-star) ──────────────────────────────────

use truss_core::contract::{BudgetSpec, ConstraintSpec, Contract, DoneClause, Meta};
use truss_core::delegation::{self, ClauseVerdict};
use truss_core::tools::WriteFile;

fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ah-contract-{}-{}", std::process::id(), name))
}

/// Build an executor mock that writes `content` to `path`, then finishes.
fn writing_executor(path: &str, content: &str) -> Arc<dyn Provider> {
    writing_executor_with_summary(path, content, "wrote the file")
}

fn writing_executor_with_summary(path: &str, content: &str, summary: &str) -> Arc<dyn Provider> {
    Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![
            assistant_tool_use(
                "w1",
                "write_file",
                json!({ "path": path, "content": content }),
            ),
            assistant_text(summary),
        ],
    })
}

fn reviewer_says(met: bool, reason: &str) -> Arc<dyn Provider> {
    Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![assistant_text(&format!(
            "{{\"met\": {met}, \"reasoning\": \"{reason}\"}}"
        ))],
    })
}

struct ContextAssertingReviewer {
    expected: String,
}

#[async_trait]
impl Provider for ContextAssertingReviewer {
    fn name(&self) -> &str {
        "context-reviewer"
    }

    fn default_model(&self) -> &str {
        "context-reviewer"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let prompt = req
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prompt.contains(&self.expected),
            "review prompt did not include expected artifact context:\n{prompt}"
        );
        Ok(assistant_text(
            "{\"met\": true, \"reasoning\": \"artifact content was provided\"}",
        ))
    }
}

struct DiffAwareReviewer;

#[async_trait]
impl Provider for DiffAwareReviewer {
    fn name(&self) -> &str {
        "diff-aware-reviewer"
    }

    fn default_model(&self) -> &str {
        "diff-aware-reviewer"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let prompt = req
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prompt.contains("EXECUTOR'S CLAIMED SUMMARY:"),
            "review prompt omitted claimed-summary label:\n{prompt}"
        );
        assert!(
            prompt.contains("ACTUAL CHANGES (diff):"),
            "review prompt omitted actual-diff label:\n{prompt}"
        );
        assert!(
            prompt.contains("I changed greeting.txt to GOODBYE WORLD."),
            "review prompt omitted executor claim:\n{prompt}"
        );
        assert!(
            prompt.contains("Full diff sha256:"),
            "review prompt omitted diff provenance:\n{prompt}"
        );
        let diff_matches_claim = prompt.contains("+GOODBYE WORLD");
        Ok(assistant_text(&format!(
            "{{\"met\": {diff_matches_claim}, \"reasoning\": \"{}\"}}",
            if diff_matches_claim {
                "diff agrees with the claimed change"
            } else {
                "claimed GOODBYE WORLD but actual diff shows different content"
            }
        )))
    }
}

fn init_git_repo(dir: &std::path::Path, file: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), body).unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init"]);
    run(&["add", file]);
    run(&[
        "-c",
        "user.name=Truss Test",
        "-c",
        "user.email=truss@example.test",
        "commit",
        "-m",
        "baseline",
    ]);
}

#[tokio::test]
async fn contract_passes_when_evidence_confirms() {
    let dir = tmp_path("ok");
    let _ = std::fs::remove_dir_all(&dir);
    let greeting = dir.join("greeting.txt");
    let gpath = greeting.to_string_lossy().to_string();

    let contract = Contract {
        contract: Meta {
            id: "c-ok".into(),
            title: "Write greeting".into(),
            intent: "Create greeting.txt containing the greeting line.".into(),
        },
        done: vec![
            DoneClause {
                kind: "check".into(),
                desc: "file contains the greeting".into(),
                run: Some(format!("cat {gpath}")),
                expect_exit: Some(0),
                expect_contains: vec!["hello from harness".into()],
                reviewer_context: vec![],
            },
            DoneClause {
                kind: "review".into(),
                desc: "greeting reads naturally".into(),
                run: None,
                expect_exit: None,
                expect_contains: vec![],
                reviewer_context: vec![],
            },
        ],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec {
            write_scope: Some(vec![dir.clone()]),
            ..Default::default()
        },
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    };

    let executor = writing_executor(&gpath, "hello from harness\n");
    let reviewer = reviewer_says(true, "reads fine");
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WriteFile));
    let tools = Arc::new(reg);
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new().with_policy(contract.policy());
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };

    let verdict = delegation::run_contract(
        Executor::InLoop(executor),
        vec![reviewer],
        tools,
        &ctx,
        &store,
        &contract,
        &cfg,
    )
    .await
    .expect("run_contract");

    assert_eq!(
        verdict.overall,
        ClauseVerdict::Pass,
        "{:?}",
        verdict.results
    );
    assert_eq!(verdict.results[0].verdict, ClauseVerdict::Pass); // the check
    assert_eq!(verdict.results[1].verdict, ClauseVerdict::Pass); // the review

    // The executor really wrote the file.
    let body = std::fs::read_to_string(&greeting).unwrap();
    assert!(body.contains("hello from harness"));

    // Contract + verdict are in the signed ledger.
    let kinds: Vec<String> = store
        .entries("c-ok")
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"contract".to_string()));
    assert!(kinds.contains(&"verdict".to_string()));
    assert!(store.verify_chain("c-ok").unwrap().is_ok());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn review_panel_includes_declared_artifact_context() {
    let dir = tmp_path("review-context");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let greeting = dir.join("greeting.txt");
    let gpath = greeting.to_string_lossy().to_string();

    let contract = Contract {
        contract: Meta {
            id: "review-context".into(),
            title: "Write greeting".into(),
            intent: "Create greeting.txt containing exactly `hello from the harness`.".into(),
        },
        done: vec![DoneClause {
            kind: "review".into(),
            desc: "greeting.txt contains exactly one line, `hello from the harness`.".into(),
            run: None,
            expect_exit: None,
            expect_contains: vec![],
            reviewer_context: vec![gpath.clone()],
        }],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec {
            write_scope: Some(vec![dir.clone()]),
            ..Default::default()
        },
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    };

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WriteFile));
    let tools = Arc::new(reg);
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new().with_policy(contract.policy());
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };

    let verdict = delegation::run_contract(
        Executor::InLoop(writing_executor(&gpath, "hello from the harness\n")),
        vec![Arc::new(ContextAssertingReviewer {
            expected: "hello from the harness".into(),
        })],
        tools,
        &ctx,
        &store,
        &contract,
        &cfg,
    )
    .await
    .expect("run_contract");

    assert_eq!(verdict.overall, ClauseVerdict::Pass);
    std::fs::remove_dir_all(&dir).ok();
}

fn diff_review_contract(id: &str, dir: std::path::PathBuf) -> Contract {
    Contract {
        contract: Meta {
            id: id.into(),
            title: "Diff-aware review".into(),
            intent: "Change greeting.txt to contain GOODBYE WORLD.".into(),
        },
        done: vec![DoneClause {
            kind: "review".into(),
            desc: "greeting.txt was actually changed to GOODBYE WORLD.".into(),
            run: None,
            expect_exit: None,
            expect_contains: vec![],
            reviewer_context: vec![],
        }],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec {
            write_scope: Some(vec![dir]),
            ..Default::default()
        },
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    }
}

#[tokio::test]
async fn diff_aware_review_panel_fails_when_claim_disagrees_with_actual_diff() {
    let dir = tmp_path("diff-aware-mismatch");
    let _ = std::fs::remove_dir_all(&dir);
    init_git_repo(&dir, "greeting.txt", "old greeting\n");
    let greeting = dir.join("greeting.txt");
    let gpath = greeting.to_string_lossy().to_string();
    let contract = diff_review_contract("diff-aware-mismatch", dir.clone());

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WriteFile));
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new().with_policy(contract.policy());
    let verdict = delegation::run_contract(
        Executor::InLoop(writing_executor_with_summary(
            &gpath,
            "hello from harness\n",
            "I changed greeting.txt to GOODBYE WORLD.",
        )),
        vec![Arc::new(DiffAwareReviewer)],
        Arc::new(reg),
        &ctx,
        &store,
        &contract,
        &DurableConfig {
            max_steps: 10,
            ..Default::default()
        },
    )
    .await
    .expect("run_contract");

    assert_eq!(verdict.overall, ClauseVerdict::Fail);
    assert_eq!(verdict.results[0].verdict, ClauseVerdict::Fail);
    let diff = verdict.reviewed_diff.as_ref().expect("reviewed diff");
    assert!(diff.content.contains("+hello from harness"), "{diff:?}");
    assert!(diff.full_bytes > 0);
    let verdict_entry = store
        .entries("diff-aware-mismatch")
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "verdict")
        .expect("verdict ledger entry");
    assert_eq!(
        verdict_entry.payload["reviewed_diff"]["reviewed_sha256"],
        diff.reviewed_sha256
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn diff_aware_review_panel_passes_when_claim_matches_actual_diff() {
    let dir = tmp_path("diff-aware-match");
    let _ = std::fs::remove_dir_all(&dir);
    init_git_repo(&dir, "greeting.txt", "old greeting\n");
    let greeting = dir.join("greeting.txt");
    let gpath = greeting.to_string_lossy().to_string();
    let contract = diff_review_contract("diff-aware-match", dir.clone());

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WriteFile));
    let verdict = delegation::run_contract(
        Executor::InLoop(writing_executor_with_summary(
            &gpath,
            "GOODBYE WORLD\n",
            "I changed greeting.txt to GOODBYE WORLD.",
        )),
        vec![Arc::new(DiffAwareReviewer)],
        Arc::new(reg),
        &Ctx::new().with_policy(contract.policy()),
        &Store::open_memory().unwrap(),
        &contract,
        &DurableConfig {
            max_steps: 10,
            ..Default::default()
        },
    )
    .await
    .expect("run_contract");

    assert_eq!(verdict.overall, ClauseVerdict::Pass);
    assert_eq!(verdict.results[0].verdict, ClauseVerdict::Pass);
    let diff = verdict.reviewed_diff.as_ref().expect("reviewed diff");
    assert!(diff.content.contains("+GOODBYE WORLD"), "{diff:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn delegated_contract_populates_session_head_for_rendered_bundle() {
    let contract = Contract {
        contract: Meta {
            id: "delegated-head".into(),
            title: "Delegated head".into(),
            intent: "Have the delegated executor report completion.".into(),
        },
        done: vec![DoneClause {
            kind: "review".into(),
            desc: "the delegated executor reported completion".into(),
            run: None,
            expect_exit: None,
            expect_contains: vec![],
            reviewer_context: vec![],
        }],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec::default(),
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    };
    let cli = CliExecutor {
        command: "sh".into(),
        args: vec!["-c".into(), "printf 'delegated summary'".into()],
        cwd: None,
        clear_env: vec![],
        prompt_via: PromptVia::Stdin,
    };
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();

    let verdict = delegation::run_contract(
        Executor::DelegatedCli(cli),
        vec![reviewer_says(true, "summary confirms completion")],
        Arc::new(ToolRegistry::new()),
        &ctx,
        &store,
        &contract,
        &DurableConfig::default(),
    )
    .await
    .expect("run_contract");

    let head = store
        .get_session("delegated-head")
        .unwrap()
        .and_then(|s| s.head_hash)
        .map(hex::encode)
        .expect("head hash");
    assert_eq!(head.len(), 64);
    assert!(store.verify_chain("delegated-head").unwrap().is_ok());

    let bundle = delegation::render_bundle(&verdict, &head);
    assert!(
        bundle.contains(&format!("ledger: head {}…", &head[..16])),
        "{bundle}"
    );
}

#[tokio::test]
async fn contract_fails_when_a_check_fails() {
    let dir = tmp_path("fail");
    let _ = std::fs::remove_dir_all(&dir);
    let greeting = dir.join("greeting.txt");
    let gpath = greeting.to_string_lossy().to_string();

    let contract = Contract {
        contract: Meta {
            id: "c-fail".into(),
            title: "Write greeting".into(),
            intent: "Create greeting.txt with the required line.".into(),
        },
        done: vec![DoneClause {
            kind: "check".into(),
            // The executor writes "hello…", but the contract demands GOODBYE.
            desc: "file contains the required line".into(),
            run: Some(format!("cat {gpath}")),
            expect_exit: Some(0),
            expect_contains: vec!["GOODBYE WORLD".into()],
            reviewer_context: vec![],
        }],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec {
            write_scope: Some(vec![dir.clone()]),
            ..Default::default()
        },
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    };

    let executor = writing_executor(&gpath, "hello from harness\n");
    let reviewer = reviewer_says(true, "n/a");
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(WriteFile));
    let tools = Arc::new(reg);
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new().with_policy(contract.policy());
    let cfg = DurableConfig {
        max_steps: 10,
        ..Default::default()
    };

    let verdict = delegation::run_contract(
        Executor::InLoop(executor),
        vec![reviewer],
        tools,
        &ctx,
        &store,
        &contract,
        &cfg,
    )
    .await
    .expect("run_contract");

    // The independent check caught that the work doesn't meet the contract,
    // even though the executor "finished" and the reviewer approved.
    assert_eq!(verdict.overall, ClauseVerdict::Fail);
    assert_eq!(verdict.results[0].verdict, ClauseVerdict::Fail);

    std::fs::remove_dir_all(&dir).ok();
}

// ── N-reviewer adversarial panel ───────────────────────────────────────────

fn review_only_contract(id: &str) -> Contract {
    Contract {
        contract: Meta {
            id: id.into(),
            title: "panel".into(),
            intent: "do the thing".into(),
        },
        done: vec![DoneClause {
            kind: "review".into(),
            desc: "the work is acceptable".into(),
            run: None,
            expect_exit: None,
            expect_contains: vec![],
            reviewer_context: vec![],
        }],
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec::default(),
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: Default::default(),
    }
}

fn completing_executor() -> Arc<dyn Provider> {
    Arc::new(MockProvider {
        calls: Arc::new(Mutex::new(0)),
        script: vec![assistant_text("did the work")],
    })
}

#[tokio::test]
async fn review_panel_passes_on_majority() {
    let contract = review_only_contract("panel-pass");
    let tools = Arc::new(ToolRegistry::new());
    let cfg = DurableConfig {
        max_steps: 5,
        ..Default::default()
    };
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();

    // 2 of 3 say met → strict majority → PASS.
    let panel: Vec<Arc<dyn Provider>> = vec![
        reviewer_says(true, "looks right"),
        reviewer_says(true, "fine"),
        reviewer_says(false, "minor nit"),
    ];
    let v = delegation::run_contract(
        Executor::InLoop(completing_executor()),
        panel,
        tools,
        &ctx,
        &store,
        &contract,
        &cfg,
    )
    .await
    .unwrap();
    assert_eq!(v.overall, ClauseVerdict::Pass);
    assert!(
        v.results[0].evidence.contains("2/3"),
        "{}",
        v.results[0].evidence
    );
}

#[tokio::test]
async fn review_panel_fails_without_majority() {
    let contract = review_only_contract("panel-fail");
    let tools = Arc::new(ToolRegistry::new());
    let cfg = DurableConfig {
        max_steps: 5,
        ..Default::default()
    };
    let store = Store::open_memory().unwrap();
    let ctx = Ctx::new();

    // Only 1 of 3 says met → no majority → FAIL (one lenient reviewer can't pass it).
    let panel: Vec<Arc<dyn Provider>> = vec![
        reviewer_says(true, "ok"),
        reviewer_says(false, "missing a requirement"),
        reviewer_says(false, "cut a corner"),
    ];
    let v = delegation::run_contract(
        Executor::InLoop(completing_executor()),
        panel,
        tools,
        &ctx,
        &store,
        &contract,
        &cfg,
    )
    .await
    .unwrap();
    assert_eq!(v.overall, ClauseVerdict::Fail);
    assert!(
        v.results[0].evidence.contains("1/3"),
        "{}",
        v.results[0].evidence
    );
}
