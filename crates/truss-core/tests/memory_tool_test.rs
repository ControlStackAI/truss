//! M2 memory-tool binding tests — hermetic provider scripts, no network.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use truss_core::budget::BudgetSnapshot;
use truss_core::ctx::Ctx;
use truss_core::delegation::{ClauseResult, ClauseVerdict, Verdict};
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::ProviderError;
use truss_core::events::AgentEvent;
use truss_core::memory::{
    EntryKind, LedgerRef, MemoryDraft, MemoryQuery, MemoryStore, SqliteMemoryStore, Trust,
};
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::Store;
use truss_core::tools::{memory::kind_for_path, ToolRegistry};

struct MockProvider {
    calls: Arc<Mutex<usize>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
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
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let mut n = self.calls.lock().unwrap();
        let i = *n;
        *n += 1;
        self.script
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Config(format!("script exhausted at call {i}")))
    }
}

fn memory_tool_use(input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::ToolUse {
            id: "mem-1".into(),
            name: "memory".into(),
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

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(t.into())],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn provider(
    script: Vec<CompletionResponse>,
) -> (Arc<dyn Provider>, Arc<Mutex<Vec<CompletionRequest>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    (
        Arc::new(MockProvider {
            calls: Arc::new(Mutex::new(0)),
            requests: requests.clone(),
            script,
        }),
        requests,
    )
}

fn memory_registry(memory: SqliteMemoryStore) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register_memory(Arc::new(memory));
    Arc::new(registry)
}

fn produced(seq: i64) -> LedgerRef {
    LedgerRef {
        session: "seed".into(),
        seq,
    }
}

fn draft(kind: EntryKind, key: &str, body: &str, seq: i64) -> MemoryDraft {
    MemoryDraft {
        kind,
        key: Some(key.into()),
        body: body.into(),
        produced_by: produced(seq),
        sources: Vec::new(),
    }
}

fn pass_verdict() -> Verdict {
    Verdict {
        contract_id: "memory-tool-test".into(),
        title: "memory verification".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "check".into(),
            desc: "fixture".into(),
            verdict: ClauseVerdict::Pass,
            evidence: "fixture".into(),
        }],
        executor_summary: "fixture".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

#[tokio::test]
async fn model_create_under_lessons_lands_unverified_lesson() {
    let store = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&store).unwrap();
    let tools = memory_registry(memory.clone());
    let (provider, _requests) = provider(vec![
        memory_tool_use(json!({
            "command": "create",
            "path": "/memories/lessons/x",
            "content": "Run fmt before review."
        })),
        text("done"),
    ]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);
    let cfg = DurableConfig {
        max_steps: 4,
        memory_tool: true,
        ..Default::default()
    };

    let out = durable::run(
        provider,
        tools,
        &ctx,
        &store,
        "m2-create",
        &cfg,
        Some("remember this".into()),
    )
    .await
    .unwrap();
    assert!(matches!(out, DurableOutcome::Completed { .. }));

    let entries = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            key: Some("x".into()),
            min_trust: Trust::Unverified,
            limit: 8,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, EntryKind::Lesson);
    assert_eq!(entries[0].trust, Trust::Unverified);
    assert_eq!(entries[0].provenance.produced_by.session, "m2-create");
    assert_eq!(
        entries[0].provenance.produced_by.seq, 1,
        "provenance points at the journaled model_result containing the tool call"
    );

    let mut saw_write_event = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::MemoryWrite {
            kind,
            trust,
            produced_by,
            ..
        } = event
        {
            saw_write_event = true;
            assert_eq!(kind, EntryKind::Lesson);
            assert_eq!(trust, Trust::Unverified);
            assert_eq!(produced_by.seq, 1);
        }
    }
    assert!(saw_write_event, "memory write event emitted");
}

#[tokio::test]
async fn model_view_returns_only_verified_entries() {
    let store = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&store).unwrap();
    let verified = memory
        .write(draft(
            EntryKind::Lesson,
            "verified",
            "Verified lesson is visible.",
            1,
        ))
        .unwrap();
    memory.promote(&verified, pass_verdict()).unwrap();
    memory
        .write(draft(
            EntryKind::Lesson,
            "unverified",
            "Unverified lesson is hidden.",
            2,
        ))
        .unwrap();

    let tools = memory_registry(memory);
    let (provider, _requests) = provider(vec![
        memory_tool_use(json!({
            "command": "view",
            "path": "/lessons"
        })),
        text("done"),
    ]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);
    let cfg = DurableConfig {
        max_steps: 4,
        memory_tool: true,
        ..Default::default()
    };

    durable::run(
        provider,
        tools,
        &ctx,
        &store,
        "m2-view",
        &cfg,
        Some("show memory".into()),
    )
    .await
    .unwrap();

    let content = store
        .entries("m2-view")
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "tool_result")
        .unwrap()
        .payload["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(content.contains("Verified lesson is visible."), "{content}");
    assert!(
        !content.contains("Unverified lesson is hidden."),
        "{content}"
    );

    let mut saw_recall_event = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::MemoryRecall { count, .. } = event {
            saw_recall_event = true;
            assert_eq!(count, 1);
        }
    }
    assert!(saw_recall_event, "memory recall event emitted");
}

#[tokio::test]
async fn memory_tool_flag_off_does_not_advertise_memory() {
    let store = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&store).unwrap();
    let tools = memory_registry(memory);
    let (provider, requests) = provider(vec![text("done")]);
    let ctx = Ctx::new();
    let cfg = DurableConfig {
        max_steps: 4,
        memory_tool: false,
        ..Default::default()
    };

    durable::run(
        provider,
        tools,
        &ctx,
        &store,
        "m2-flag-off",
        &cfg,
        Some("hello".into()),
    )
    .await
    .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].tools.iter().all(|tool| tool.name != "memory"),
        "memory tool must be absent when the session flag is off"
    );
}

#[test]
fn path_to_kind_inference_table() {
    let cases = [
        ("/memories/goal/contract", EntryKind::Goal),
        ("/decisions/sqlite", EntryKind::Decision),
        ("/lessons/fmt", EntryKind::Lesson),
        ("/memories/lessons/fmt", EntryKind::Lesson),
        ("/facts/rust", EntryKind::Fact),
        ("/artifacts/report", EntryKind::Artifact),
        ("/scratch/notes", EntryKind::Scratch),
        ("/whatever", EntryKind::Scratch),
    ];

    for (path, expected) in cases {
        assert_eq!(kind_for_path(path), expected, "{path}");
    }
}
