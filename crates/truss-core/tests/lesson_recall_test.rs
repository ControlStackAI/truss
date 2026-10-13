use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use truss_core::budget::BudgetSnapshot;
use truss_core::ctx::Ctx;
use truss_core::delegation::{apply_memory_verdict, ClauseResult, ClauseVerdict, Verdict};
use truss_core::durable::{self, DurableConfig, DurableOutcome};
use truss_core::memory::{
    EntryKind, LedgerRef, MemoryConfig, MemoryDraft, MemoryStore, SqliteMemoryStore,
};
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::Store;
use truss_core::tools::ToolRegistry;

#[derive(Clone, Default)]
struct CaptureProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl Provider for CaptureProvider {
    fn name(&self) -> &str {
        "capture"
    }

    fn default_model(&self) -> &str {
        "capture"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, truss_core::error::ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        Ok(CompletionResponse {
            content: vec![Block::Text("done".into())],
            stop_reason: StopReason::EndTurn,
            model: "capture".into(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
        })
    }
}

fn verdict() -> Verdict {
    Verdict {
        contract_id: "lesson-recall-verifier".into(),
        title: "lesson recall verifier".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "review".into(),
            desc: "lesson is supported by evidence".into(),
            verdict: ClauseVerdict::Pass,
            evidence: "fixture".into(),
        }],
        executor_summary: "fixture".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

fn create_verified_lesson(
    durable: &Store,
    memory: &SqliteMemoryStore,
    key: &str,
    body: &str,
) -> truss_core::memory::MemoryId {
    durable
        .create_session(key, Some("fixture"), "local", "local")
        .unwrap();
    let produced = durable
        .append(
            key,
            "verdict",
            &serde_json::json!({ "summary": "produced" }),
        )
        .unwrap();
    let id = memory
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some(key.into()),
            body: body.into(),
            produced_by: LedgerRef {
                session: key.into(),
                seq: produced.seq,
            },
            sources: Vec::new(),
        })
        .unwrap();
    apply_memory_verdict(memory, Some(durable), &id, verdict()).unwrap();
    id
}

fn create_verified_playbook(
    durable: &Store,
    memory: &SqliteMemoryStore,
    key: &str,
    body: &str,
) -> truss_core::memory::MemoryId {
    durable
        .create_session(key, Some("fixture"), "local", "local")
        .unwrap();
    let produced = durable
        .append(
            key,
            "playbook_imported",
            &serde_json::json!({ "summary": "produced" }),
        )
        .unwrap();
    let id = memory
        .write(MemoryDraft {
            kind: EntryKind::Playbook,
            key: Some(key.into()),
            body: body.into(),
            produced_by: LedgerRef {
                session: key.into(),
                seq: produced.seq,
            },
            sources: Vec::new(),
        })
        .unwrap();
    apply_memory_verdict(memory, Some(durable), &id, verdict()).unwrap();
    id
}

async fn run_with_memory(
    durable: &Store,
    memory: Arc<SqliteMemoryStore>,
    config: MemoryConfig,
    prompt: &str,
) -> Vec<CompletionRequest> {
    static RUN_ID: AtomicUsize = AtomicUsize::new(0);
    let provider = CaptureProvider::default();
    let requests = provider.requests.clone();
    let outcome = durable::run(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        &Ctx::new(),
        durable,
        &format!("recall-run-{}", RUN_ID.fetch_add(1, Ordering::SeqCst)),
        &DurableConfig {
            max_steps: 1,
            memory: config,
            memory_store: Some(memory),
            ..DurableConfig::default()
        },
        Some(prompt.into()),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DurableOutcome::Completed { .. }));
    let captured = requests.lock().unwrap().clone();
    captured
}

fn request_text(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .map(|message| message.text())
        .collect::<Vec<_>>()
        .join("\n---\n")
}

#[tokio::test]
async fn verified_relevant_lesson_is_injected_at_run_start_with_provenance() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    create_verified_lesson(
        &durable,
        &memory,
        "cargo-fmt",
        "Run cargo fmt before opening a Rust PR.",
    );

    let requests = run_with_memory(
        &durable,
        memory.clone(),
        MemoryConfig::default(),
        "Please prepare a Rust PR and remember cargo fmt.",
    )
    .await;
    let text = request_text(&requests[0]);

    assert!(text.contains("[L2 verified learned lessons]"), "{text}");
    assert!(
        text.contains("Run cargo fmt before opening a Rust PR."),
        "{text}"
    );
    assert!(text.contains("[trust: verified]"), "{text}");
    assert!(
        text.contains("provenance: produced_by cargo-fmt#0; verified_by cargo-fmt#1"),
        "{text}"
    );
    assert!(durable.verify_chain("cargo-fmt").unwrap().is_ok());
    assert_eq!(
        memory
            .get(&memory.query(&Default::default()).unwrap()[0].id)
            .unwrap()
            .provenance
            .verified_by_ref
            .as_ref()
            .unwrap()
            .seq,
        1
    );
}

#[tokio::test]
async fn unverified_and_stale_lessons_are_excluded_from_injected_context() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    create_verified_lesson(
        &durable,
        &memory,
        "verified-rust",
        "Rust tasks should run cargo fmt before review.",
    );
    memory
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some("unverified-rust".into()),
            body: "Unverified Rust lesson must stay out of context.".into(),
            produced_by: LedgerRef {
                session: "unverified-rust".into(),
                seq: 0,
            },
            sources: Vec::new(),
        })
        .unwrap();
    let stale = create_verified_lesson(
        &durable,
        &memory,
        "stale-rust",
        "Stale Rust lesson must stay out of context.",
    );
    memory.mark_stale(&stale, None).unwrap();

    let requests = run_with_memory(
        &durable,
        memory,
        MemoryConfig::default(),
        "Work on a Rust task and run cargo fmt.",
    )
    .await;
    let text = request_text(&requests[0]);

    assert!(
        text.contains("Rust tasks should run cargo fmt before review."),
        "{text}"
    );
    assert!(
        !text.contains("Unverified Rust lesson must stay out of context."),
        "{text}"
    );
    assert!(
        !text.contains("Stale Rust lesson must stay out of context."),
        "{text}"
    );
}

#[tokio::test]
async fn recall_honors_count_and_token_budget_caps() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    for idx in 0..3 {
        create_verified_lesson(
            &durable,
            &memory,
            &format!("rust-cap-{idx}"),
            &format!("Rust bounded lesson {idx}: keep the recall set bounded."),
        );
    }

    let requests = run_with_memory(
        &durable,
        memory.clone(),
        MemoryConfig {
            lesson_recall_max_lessons: 2,
            lesson_recall_token_budget: 1_000,
            ..MemoryConfig::default()
        },
        "Use Rust cap lessons for this task.",
    )
    .await;
    let text = request_text(&requests[0]);
    assert_eq!(text.matches("Rust bounded lesson").count(), 2, "{text}");

    let requests = run_with_memory(
        &durable,
        memory,
        MemoryConfig {
            lesson_recall_max_lessons: 3,
            lesson_recall_token_budget: 8,
            ..MemoryConfig::default()
        },
        "Use Rust cap lessons for this task.",
    )
    .await;
    let text = request_text(&requests[0]);
    assert_eq!(text.matches("Rust bounded lesson").count(), 0, "{text}");
}

#[tokio::test]
async fn zero_verified_lessons_means_no_l2_recall_block() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    memory
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some("draft".into()),
            body: "Draft lesson is not verified.".into(),
            produced_by: LedgerRef {
                session: "draft".into(),
                seq: 0,
            },
            sources: Vec::new(),
        })
        .unwrap();

    let requests = run_with_memory(
        &durable,
        memory,
        MemoryConfig::default(),
        "Work on a Rust task.",
    )
    .await;
    assert_eq!(requests[0].messages.len(), 1);
    let text = request_text(&requests[0]);
    assert!(!text.contains("[L2 verified learned lessons]"), "{text}");
    assert!(!text.contains("Draft lesson is not verified."), "{text}");
}

#[tokio::test]
async fn verified_playbook_recall_is_opt_in_and_injected_with_provenance() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    create_verified_playbook(
        &durable,
        &memory,
        "rust-review-playbook",
        "Rust review playbook: run cargo fmt and inspect the signed ledger.",
    );

    let requests = run_with_memory(
        &durable,
        memory.clone(),
        MemoryConfig::default(),
        "Use the Rust review playbook for this task.",
    )
    .await;
    let text = request_text(&requests[0]);
    assert!(!text.contains("[L2 verified playbooks]"), "{text}");
    assert!(
        !text.contains("Rust review playbook: run cargo fmt"),
        "{text}"
    );

    let requests = run_with_memory(
        &durable,
        memory,
        MemoryConfig {
            playbook_recall_max_playbooks: 2,
            playbook_recall_token_budget: 512,
            ..MemoryConfig::default()
        },
        "Use the Rust review playbook for this task.",
    )
    .await;
    let text = request_text(&requests[0]);
    assert!(text.contains("[L2 verified playbooks]"), "{text}");
    assert!(
        text.contains("Rust review playbook: run cargo fmt and inspect the signed ledger."),
        "{text}"
    );
    assert!(text.contains("[trust: verified]"), "{text}");
    assert!(
        text.contains(
            "provenance: produced_by rust-review-playbook#0; verified_by rust-review-playbook#1"
        ),
        "{text}"
    );
}

#[tokio::test]
async fn unverified_playbooks_are_excluded_from_trusted_recall() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    create_verified_playbook(
        &durable,
        &memory,
        "verified-playbook",
        "Verified playbook says to run cargo fmt before review.",
    );
    memory
        .write(MemoryDraft {
            kind: EntryKind::Playbook,
            key: Some("draft-playbook".into()),
            body: "Draft playbook must never appear as trusted recall.".into(),
            produced_by: LedgerRef {
                session: "draft-playbook".into(),
                seq: 0,
            },
            sources: Vec::new(),
        })
        .unwrap();

    let recall = truss_core::memory::recall_verified_playbooks(
        memory.as_ref(),
        "Use the verified playbook for cargo fmt review.",
        &MemoryConfig {
            playbook_recall_max_playbooks: 4,
            playbook_recall_token_budget: 512,
            ..MemoryConfig::default()
        },
    )
    .unwrap()
    .expect("verified playbook recall");
    assert_eq!(recall.entries.len(), 1);
    assert_eq!(recall.entries[0].key.as_deref(), Some("verified-playbook"));
    assert!(!recall.entries[0].body.contains("Draft playbook"));

    let requests = run_with_memory(
        &durable,
        memory,
        MemoryConfig {
            playbook_recall_max_playbooks: 4,
            playbook_recall_token_budget: 512,
            ..MemoryConfig::default()
        },
        "Use the verified playbook for cargo fmt review.",
    )
    .await;
    let text = request_text(&requests[0]);
    assert!(
        text.contains("Verified playbook says to run cargo fmt"),
        "{text}"
    );
    assert!(
        !text.contains("Draft playbook must never appear as trusted recall."),
        "{text}"
    );
}

#[test]
fn recalled_lesson_retains_verifying_verdict_ref_and_signed_chain_validates() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let id = create_verified_lesson(
        &durable,
        &memory,
        "provenance-rust",
        "Rust provenance lessons should carry their verifier ledger ref.",
    );

    let recall = truss_core::memory::recall_verified_lessons(
        &memory,
        "Need Rust provenance lessons for this task.",
        &MemoryConfig::default(),
    )
    .unwrap()
    .expect("verified lesson recall");
    assert_eq!(recall.entries.len(), 1);
    assert_eq!(recall.entries[0].id, id);
    assert_eq!(
        recall.entries[0].provenance.verified_by_ref,
        Some(LedgerRef {
            session: "provenance-rust".into(),
            seq: 1,
        })
    );
    assert!(durable.verify_chain("provenance-rust").unwrap().is_ok());
}
