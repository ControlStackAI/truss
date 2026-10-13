use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use proptest::prelude::*;
use serde_json::json;

use truss_core::budget::{BudgetLimits, BudgetMeter, BudgetSnapshot};
use truss_core::ctx::Ctx;
use truss_core::delegation::{apply_memory_verdict, ClauseResult, ClauseVerdict, Verdict};
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::error::{ProviderError, ToolError};
use truss_core::memory::{
    EntryKind, LedgerRef, MemoryDraft, MemoryId, MemoryQuery, MemoryStore, SqliteMemoryStore, Trust,
};
use truss_core::message::{Block, Message};
use truss_core::provider::{
    CompletionRequest, CompletionResponse, Provider, StopReason, ToolDef, Usage,
};
use truss_core::replay::reconstruct;
use truss_core::store::Store;
use truss_core::tools::{Tool, ToolRegistry};

const SESSION: &str = "invariant-session";

fn verdict(overall: ClauseVerdict, label: &str) -> Verdict {
    Verdict {
        contract_id: format!("contract-{label}"),
        title: "property panel".into(),
        overall: overall.clone(),
        results: vec![ClauseResult {
            kind: "review".into(),
            desc: "claim is supported".into(),
            verdict: overall,
            evidence: "property fixture panel".into(),
        }],
        executor_summary: "property fixture output".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

fn setup_memory() -> (Store, SqliteMemoryStore) {
    let durable = Store::open_memory().unwrap();
    durable
        .create_session(SESSION, Some("property"), "local", "local")
        .unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    (durable, memory)
}

fn write_memory(durable: &Store, memory: &SqliteMemoryStore, index: usize) -> MemoryId {
    let produced = durable
        .append(
            SESSION,
            "memory_write",
            &json!({ "index": index, "body": format!("body-{index}") }),
        )
        .unwrap();
    let produced_by = LedgerRef {
        session: SESSION.into(),
        seq: produced.seq,
    };
    memory
        .write(MemoryDraft {
            kind: if index.is_multiple_of(2) {
                EntryKind::Lesson
            } else {
                EntryKind::Fact
            },
            key: Some(format!("key-{index}")),
            body: format!("property memory {index}"),
            produced_by: produced_by.clone(),
            sources: vec![produced_by],
        })
        .unwrap()
}

fn exercise_memory_ops(ops: &[u8]) -> (Store, SqliteMemoryStore, Vec<MemoryId>) {
    let (durable, memory) = setup_memory();
    let mut ids = Vec::new();

    for (index, op) in ops.iter().enumerate() {
        match op % 4 {
            0 => ids.push(write_memory(&durable, &memory, index)),
            1 => {
                if let Some(id) = ids.last() {
                    apply_memory_verdict(
                        &memory,
                        Some(&durable),
                        id,
                        verdict(ClauseVerdict::Pass, &format!("pass-{index}")),
                    )
                    .unwrap();
                }
            }
            2 => {
                if let Some(id) = ids.last() {
                    apply_memory_verdict(
                        &memory,
                        Some(&durable),
                        id,
                        verdict(ClauseVerdict::Fail, &format!("fail-{index}")),
                    )
                    .unwrap();
                }
            }
            _ => {
                if let Some(id) = ids.last() {
                    memory.mark_stale(id, None).unwrap();
                }
            }
        }
    }

    (durable, memory, ids)
}

fn ledger_ref_exists(store: &Store, ledger: &LedgerRef) -> bool {
    store
        .entries(&ledger.session)
        .unwrap()
        .into_iter()
        .any(|entry| entry.seq == ledger.seq)
}

fn journal_fingerprint(store: &Store, session_id: &str) -> serde_json::Value {
    let entries = store.entries(session_id).unwrap();
    serde_json::to_value(
        entries
            .iter()
            .map(|entry| {
                json!({
                    "seq": entry.seq,
                    "kind": entry.kind,
                    "payload": entry.payload,
                    "prev_hash": hex::encode(&entry.prev_hash),
                    "entry_hash": hex::encode(&entry.entry_hash),
                    "signature": hex::encode(&entry.signature),
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

fn head_hash(store: &Store, session_id: &str) -> Option<Vec<u8>> {
    store.get_session(session_id).unwrap().unwrap().head_hash
}

fn seed_projectable_journal(session_id: &str, tool_pairs: usize) -> Store {
    let store = Store::open_memory().unwrap();
    store
        .create_session(session_id, Some("mock-model"), "local", "local")
        .unwrap();
    store
        .append(
            session_id,
            "run_started",
            &json!({ "prompt": "authoritative task" }),
        )
        .unwrap();

    for index in 0..tool_pairs {
        let call_id = format!("c{index}");
        let tool_use = vec![Block::ToolUse {
            id: call_id.clone(),
            name: "ping".into(),
            input: json!({ "index": index }),
        }];
        store
            .append(
                session_id,
                "model_result",
                &json!({
                    "content": serde_json::to_value(&tool_use).unwrap(),
                    "stop_reason": "ToolUse",
                    "usage": { "input": 10 + index as u64, "output": 1 },
                }),
            )
            .unwrap();
        store
            .append(
                session_id,
                "tool_result",
                &json!({
                    "call_id": call_id,
                    "name": "ping",
                    "input": { "index": index },
                    "content": format!("pong-{index}"),
                    "is_error": false,
                }),
            )
            .unwrap();
    }

    store
}

fn message_fingerprint(messages: &[Message]) -> serde_json::Value {
    serde_json::to_value(messages).unwrap()
}

fn assistant_text(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(text.into())],
        stop_reason: StopReason::EndTurn,
        model: "mock-model".into(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 1,
        },
    }
}

struct CapturingProvider {
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    script: Vec<CompletionResponse>,
}

#[async_trait]
impl Provider for CapturingProvider {
    fn name(&self) -> &str {
        "capturing-mock"
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
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.script
            .get(index)
            .cloned()
            .ok_or_else(|| ProviderError::Config(format!("script exhausted at call {index}")))
    }
}

struct CountingPingTool {
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingPingTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "ping".into(),
            description: "counting test tool".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, _ctx: &Ctx, _input: serde_json::Value) -> Result<String, ToolError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok("live-pong".into())
    }
}

#[test]
fn projection_never_mutates_recorded_truth() {
    let session_id = "projection-view-journal-authority";
    let store = seed_projectable_journal(session_id, 8);
    let before_journal = journal_fingerprint(&store, session_id);
    let before_head = head_hash(&store, session_id);
    let before_rec = reconstruct(&store.entries(session_id).unwrap());
    assert_eq!(before_rec.messages.len(), 17);

    for keep_tail in [1, 2, 4, 8] {
        let projected = truss_core::projection::project(&before_rec.messages, keep_tail);
        assert!(
            projected.len() < before_rec.messages.len(),
            "keep_tail={keep_tail} should compact this fixture"
        );

        let after_rec = reconstruct(&store.entries(session_id).unwrap());
        assert_eq!(
            message_fingerprint(&after_rec.messages),
            message_fingerprint(&before_rec.messages),
            "projection must not truncate reconstructable journal truth"
        );
        assert_eq!(journal_fingerprint(&store, session_id), before_journal);
        assert_eq!(head_hash(&store, session_id), before_head);
    }
}

#[test]
fn projection_is_deterministic_view_not_journal_state() {
    let session_id = "projection-pure-view";
    let store = seed_projectable_journal(session_id, 7);
    let before_journal = journal_fingerprint(&store, session_id);
    let before_head = head_hash(&store, session_id);
    let messages = reconstruct(&store.entries(session_id).unwrap()).messages;

    let first = message_fingerprint(&truss_core::projection::project(&messages, 3));
    let second = message_fingerprint(&truss_core::projection::project(&messages, 3));
    assert_eq!(
        first, second,
        "same journal must produce identical projection"
    );

    let aggressive = message_fingerprint(&truss_core::projection::project(&messages, 2));
    let conservative = message_fingerprint(&truss_core::projection::project(&messages, 5));
    assert_ne!(
        aggressive, conservative,
        "projection aggressiveness should only change model-facing context"
    );
    assert_eq!(journal_fingerprint(&store, session_id), before_journal);
    assert_eq!(head_hash(&store, session_id), before_head);
    assert_eq!(store.verify_chain(session_id).unwrap(), Ok(15));
}

#[tokio::test]
async fn projection_enabled_resume_replays_journal_not_summary() {
    let session_id = "projection-enabled-resume";
    let store = seed_projectable_journal(session_id, 7);
    let before_journal = journal_fingerprint(&store, session_id);
    let before_messages = reconstruct(&store.entries(session_id).unwrap()).messages;

    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        calls: provider_calls.clone(),
        requests: requests.clone(),
        script: vec![assistant_text("finished from live frontier")],
    });

    let tool_hits = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(CountingPingTool {
        hits: tool_hits.clone(),
    }));
    let tools = Arc::new(registry);
    let cfg = DurableConfig {
        max_steps: 4,
        max_context_msgs: 3,
        ..Default::default()
    };

    let outcome = durable::run(provider, tools, &Ctx::new(), &store, session_id, &cfg, None)
        .await
        .expect("projection-enabled resume");

    match outcome {
        DurableOutcome::Completed { final_text } => {
            assert_eq!(final_text, "finished from live frontier");
        }
        other => panic!("expected completion, got {other:?}"),
    }
    assert_eq!(
        tool_hits.load(Ordering::SeqCst),
        0,
        "recorded tool results must replay from the journal, not execute from projection"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].messages.len(),
        5,
        "resume should feed only Head + summary + tail to the model"
    );
    assert!(
        captured[0].messages[1]
            .text()
            .contains("earlier turns elided"),
        "projection summary should be model-facing only"
    );

    let after_messages = reconstruct(&store.entries(session_id).unwrap()).messages;
    assert_eq!(
        &message_fingerprint(&after_messages[..before_messages.len()]),
        &message_fingerprint(&before_messages),
        "resume must preserve the full recorded prefix despite projected context"
    );
    assert_eq!(store.verify_chain(session_id).unwrap(), Ok(17));
    assert_eq!(
        &journal_fingerprint(&store, session_id).as_array().unwrap()
            [..before_journal.as_array().unwrap().len()],
        before_journal.as_array().unwrap().as_slice(),
        "resume must append after the authoritative journal prefix"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn verified_memory_always_has_signed_panel_verdict(ops in prop::collection::vec(0u8..32, 0..64)) {
        let (durable, memory, ids) = exercise_memory_ops(&ops);

        for id in ids {
            let entry = memory.get(&id).unwrap();
            if entry.trust == Trust::Verified {
                prop_assert!(entry.provenance.verified_by.is_some());
                let verdict_ref = entry.provenance.verified_by_ref.as_ref();
                prop_assert!(verdict_ref.is_some());
                let verdict_ref = verdict_ref.unwrap();
                prop_assert!(ledger_ref_exists(&durable, verdict_ref));
                let verdict_entry = durable
                    .entries(&verdict_ref.session)
                    .unwrap()
                    .into_iter()
                    .find(|entry| entry.seq == verdict_ref.seq)
                    .unwrap();
                prop_assert_eq!(verdict_entry.kind, "memory_verdict");
                prop_assert!(!verdict_entry.signature.is_empty());
            }
        }
        prop_assert!(durable.verify_chain(SESSION).unwrap().is_ok());
    }

    #[test]
    fn ledger_hash_chain_validates_after_any_write_sequence(
        payloads in prop::collection::vec((0u8..8, any::<i64>()), 0..32)
    ) {
        let store = Store::open_memory().unwrap();
        store
            .create_session("ledger-any-write", Some("property"), "local", "local")
            .unwrap();

        for (kind_index, value) in &payloads {
            store
                .append(
                    "ledger-any-write",
                    &format!("kind-{kind_index}"),
                    &json!({ "value": value }),
                )
                .unwrap();
        }

        prop_assert_eq!(
            store.verify_chain("ledger-any-write").unwrap(),
            Ok(payloads.len())
        );
    }

    #[test]
    fn branch_budget_usage_never_exceeds_parent_step_ceiling(
        parent_ceiling in 0u64..128,
        branches in prop::collection::vec((0u64..64, 0u64..96), 0..48)
    ) {
        let parent = BudgetMeter::new(BudgetLimits {
            max_steps: Some(parent_ceiling),
            ..Default::default()
        });

        for (slice_ceiling, attempts) in branches {
            let child = parent.child_slice(BudgetLimits {
                max_steps: Some(slice_ceiling),
                ..Default::default()
            });
            for _ in 0..attempts {
                child.record_step();
            }
            prop_assert!(child.slice_snapshot().steps <= slice_ceiling);
            prop_assert!(parent.snapshot().steps <= parent_ceiling);
        }
    }

    #[test]
    fn default_memory_query_never_surfaces_unverified_rejected_or_stale(
        ops in prop::collection::vec(0u8..32, 0..64)
    ) {
        let (_durable, memory, _ids) = exercise_memory_ops(&ops);

        let trusted = memory.query(&MemoryQuery::default()).unwrap();
        prop_assert!(trusted.iter().all(|entry| entry.trust == Trust::Verified));
    }

    #[test]
    fn every_stored_entry_has_resolvable_provenance(
        ops in prop::collection::vec(0u8..32, 0..64)
    ) {
        let (durable, memory, ids) = exercise_memory_ops(&ops);

        for id in ids {
            let entry = memory.get(&id).unwrap();
            prop_assert!(ledger_ref_exists(&durable, &entry.provenance.produced_by));
            for source in &entry.provenance.sources {
                prop_assert!(ledger_ref_exists(&durable, source));
            }
            if entry.trust == Trust::Verified {
                let verdict_ref = entry.provenance.verified_by_ref.as_ref().unwrap();
                prop_assert!(ledger_ref_exists(&durable, verdict_ref));
            }
        }
        prop_assert!(durable.verify_chain(SESSION).unwrap().is_ok());
    }
}
