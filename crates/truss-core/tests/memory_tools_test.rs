use std::sync::Arc;

use serde_json::{json, Value};
use truss_core::budget::BudgetSnapshot;
use truss_core::ctx::Ctx;
use truss_core::delegation::{apply_memory_verdict, ClauseResult, ClauseVerdict, Verdict};
use truss_core::memory::{
    DeterministicEmbedder, EntryKind, LedgerRef, MemoryDraft, MemoryRecallMode, MemoryStore,
    MemoryStoreOptions, SqliteMemoryStore,
};
use truss_core::store::{tool_call_key, Store};
use truss_core::tools::{MemoryReadJournal, ToolRegistry};

const SESSION: &str = "memory-tools";

fn setup_exact() -> (Store, SqliteMemoryStore) {
    let store = Store::open_memory().unwrap();
    store
        .create_session(SESSION, Some("mock"), "local", "local")
        .unwrap();
    let memory = SqliteMemoryStore::from_store(&store).unwrap();
    (store, memory)
}

fn setup_hybrid() -> (Store, SqliteMemoryStore) {
    let store = Store::open_memory().unwrap();
    store
        .create_session(SESSION, Some("mock"), "local", "local")
        .unwrap();
    let memory = SqliteMemoryStore::from_store_with_options(
        &store,
        MemoryStoreOptions::hybrid(Arc::new(DeterministicEmbedder::new(32))),
    )
    .unwrap();
    (store, memory)
}

fn produced_by(store: &Store, seq_hint: i64) -> LedgerRef {
    let entry = store
        .append(SESSION, "seed_memory", &json!({ "seq_hint": seq_hint }))
        .unwrap();
    LedgerRef {
        session: SESSION.into(),
        seq: entry.seq,
    }
}

fn write_memory(
    store: &Store,
    memory: &SqliteMemoryStore,
    kind: EntryKind,
    key: &str,
    body: &str,
    seq_hint: i64,
) -> truss_core::memory::MemoryId {
    let produced_by = produced_by(store, seq_hint);
    memory
        .write(MemoryDraft {
            kind,
            key: Some(key.into()),
            body: body.into(),
            produced_by: produced_by.clone(),
            sources: vec![produced_by],
        })
        .unwrap()
}

fn verify(store: &Store, memory: &SqliteMemoryStore, id: &truss_core::memory::MemoryId) {
    apply_memory_verdict(memory, Some(store), id, pass_verdict()).unwrap();
}

fn pass_verdict() -> Verdict {
    Verdict {
        contract_id: "memory-tools-test".into(),
        title: "memory verification".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "check".into(),
            desc: "memory claim is supported".into(),
            verdict: ClauseVerdict::Pass,
            evidence: "fixture evidence".into(),
        }],
        executor_summary: "verified by fixture".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

fn registry(store: &Store, memory: SqliteMemoryStore, recall: MemoryRecallMode) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_memory_read_tools(
        Arc::new(memory),
        Some(MemoryReadJournal {
            store: store.clone(),
            session_id: SESSION.into(),
        }),
        recall,
    );
    registry
}

#[tokio::test]
async fn memory_query_defaults_to_verified_only_and_surfaces_provenance() {
    let (store, memory) = setup_exact();
    let unverified = write_memory(
        &store,
        &memory,
        EntryKind::Lesson,
        "rust-unverified",
        "Rust unverified note should stay hidden.",
        1,
    );
    let verified = write_memory(
        &store,
        &memory,
        EntryKind::Lesson,
        "rust-verified",
        "Rust verified lesson should be recalled.",
        2,
    );
    verify(&store, &memory, &verified);

    let registry = registry(&store, memory.clone(), MemoryRecallMode::Exact);
    let out = registry
        .call_with_ledger(
            &Ctx::new(),
            "memory_query",
            json!({ "query": "Rust", "kind": "lesson", "limit": 10 }),
            None,
        )
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();

    assert_eq!(value["min_trust"], "verified");
    assert_eq!(value["count"], 1);
    assert_eq!(
        value["entries"][0]["id"].as_str(),
        Some(verified.0.as_str())
    );
    assert_eq!(value["entries"][0]["kind"], "lesson");
    assert_eq!(value["entries"][0]["trust"], "verified");
    assert_eq!(
        value["entries"][0]["ledger_ref"],
        value["entries"][0]["provenance"]["produced_by"]
    );
    assert!(value["entries"][0]["provenance"]["verified_by_ref"].is_object());
    assert_eq!(memory.get(&unverified).unwrap().trust.rank(), 1);

    let provenance = store
        .entries(SESSION)
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "memory_tool_call")
        .expect("signed memory retrieval observation");
    assert_eq!(provenance.payload["tool"], "memory_query");
    assert_eq!(provenance.payload["side_effect"], false);
    assert!(store.verify_chain(SESSION).unwrap().is_ok());
}

#[tokio::test]
async fn memory_view_defaults_to_verified_but_can_widen_trust_floor() {
    let (store, memory) = setup_exact();
    let unverified = write_memory(
        &store,
        &memory,
        EntryKind::Fact,
        "draft",
        "Draft fact is visible only with explicit opt-in.",
        1,
    );
    let registry = registry(&store, memory, MemoryRecallMode::Exact);

    let default_out = registry
        .call_with_ledger(
            &Ctx::new(),
            "memory_view",
            json!({ "id": unverified.0.clone() }),
            None,
        )
        .await
        .unwrap();
    let default_value: Value = serde_json::from_str(&default_out).unwrap();
    assert_eq!(default_value["count"], 0);
    assert_eq!(
        default_value["filtered"][0].as_str(),
        Some(unverified.0.as_str())
    );

    let widened_out = registry
        .call_with_ledger(
            &Ctx::new(),
            "memory_view",
            json!({ "id": unverified.0.clone(), "min_trust": "unverified", "revalidate": true }),
            None,
        )
        .await
        .unwrap();
    let widened_value: Value = serde_json::from_str(&widened_out).unwrap();
    assert_eq!(widened_value["min_trust"], "unverified");
    assert_eq!(widened_value["count"], 1);
    assert_eq!(
        widened_value["entries"][0]["id"].as_str(),
        Some(unverified.0.as_str())
    );
}

#[tokio::test]
async fn memory_query_replays_signed_memo_and_revalidates_on_request() {
    let (store, memory) = setup_exact();
    let first = write_memory(
        &store,
        &memory,
        EntryKind::Lesson,
        "first",
        "Rust replay lesson one.",
        1,
    );
    verify(&store, &memory, &first);
    let registry = registry(&store, memory.clone(), MemoryRecallMode::Exact);
    let input = json!({ "query": "Rust replay", "kind": "lesson" });

    let first_out = registry
        .call_with_ledger(&Ctx::new(), "memory_query", input.clone(), None)
        .await
        .unwrap();
    let second = write_memory(
        &store,
        &memory,
        EntryKind::Lesson,
        "second",
        "Rust replay lesson two.",
        2,
    );
    verify(&store, &memory, &second);

    let replay = registry
        .call_with_ledger(&Ctx::new(), "memory_query", input.clone(), None)
        .await
        .unwrap();
    assert_eq!(first_out, replay, "memo replay returns the original recall");

    let revalidated = registry
        .call_with_ledger(
            &Ctx::new(),
            "memory_query",
            json!({ "query": "Rust replay", "kind": "lesson", "revalidate": true }),
            None,
        )
        .await
        .unwrap();
    let revalidated_value: Value = serde_json::from_str(&revalidated).unwrap();
    assert_eq!(revalidated_value["count"], 2);
    assert_eq!(
        store
            .entries(SESSION)
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == "memory_tool_call")
            .count(),
        2
    );
}

#[tokio::test]
async fn memory_query_rejects_tampered_memo_without_signed_observation() {
    let (store, memory) = setup_exact();
    let id = write_memory(
        &store,
        &memory,
        EntryKind::Lesson,
        "tamper",
        "Rust tamper lesson.",
        1,
    );
    verify(&store, &memory, &id);
    let registry = registry(&store, memory, MemoryRecallMode::Exact);
    let input = json!({ "query": "Rust tamper", "kind": "lesson" });
    registry
        .call_with_ledger(&Ctx::new(), "memory_query", input.clone(), None)
        .await
        .unwrap();

    let memo_key = tool_call_key("memory_query", &input);
    store
        .memo_put(SESSION, &memo_key, "{\"count\":99}", false)
        .unwrap();
    let err = registry
        .call_with_ledger(&Ctx::new(), "memory_query", input, None)
        .await
        .expect_err("tampered memo must not replay");
    assert!(err
        .to_string()
        .contains("memory memo missing matching signed provenance"));
}

#[tokio::test]
async fn memory_query_honors_hybrid_recall_mode() {
    let (store, memory) = setup_hybrid();
    let semantic_only = write_memory(
        &store,
        &memory,
        EntryKind::Fact,
        "semantic",
        "Needle-adjacent durable memory content.",
        1,
    );
    let exact = write_memory(
        &store,
        &memory,
        EntryKind::Fact,
        "exact",
        "The exact hybrid needle should rank first.",
        2,
    );
    verify(&store, &memory, &semantic_only);
    verify(&store, &memory, &exact);
    let registry = registry(&store, memory, MemoryRecallMode::Hybrid);

    let out = registry
        .call_with_ledger(
            &Ctx::new(),
            "memory_query",
            json!({ "query": "hybrid needle", "kind": "fact", "limit": 2 }),
            None,
        )
        .await
        .unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["recall_mode"], "hybrid");
    assert_eq!(value["entries"][0]["id"].as_str(), Some(exact.0.as_str()));
}
