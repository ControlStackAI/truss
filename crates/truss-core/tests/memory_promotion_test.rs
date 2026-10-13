use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use truss_core::budget::BudgetSnapshot;
use truss_core::ctx::Ctx;
use truss_core::delegation::{ClauseResult, ClauseVerdict, Verdict};
use truss_core::error::ProviderError;
use truss_core::memory::{
    EffectiveBudgetPolicyLayer, EntryKind, IdentityPolicy, LedgerRef, LessonExtractionPolicy,
    MemoryConfig, MemoryConfigLayer, MemoryDraft, MemoryQuery, MemoryStore, SqliteMemoryStore,
    Trust, VerificationPolicy,
};
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::Store;
use truss_core::tools::ToolRegistry;

#[derive(Clone)]
struct ReviewProvider {
    met: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ReviewProvider {
    fn name(&self) -> &str {
        "review-fixture"
    }

    fn default_model(&self) -> &str {
        "review-fixture"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: vec![Block::Text(format!(
                "{{\"met\": {}, \"reasoning\": \"fixture\"}}",
                self.met
            ))],
            stop_reason: StopReason::EndTurn,
            model: "review-fixture".into(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
        })
    }
}

fn produced(seq: i64) -> LedgerRef {
    LedgerRef {
        session: "m3".into(),
        seq,
    }
}

fn draft(kind: EntryKind, key: Option<&str>, body: &str, seq: i64) -> MemoryDraft {
    MemoryDraft {
        kind,
        key: key.map(str::to_string),
        body: body.into(),
        produced_by: produced(seq),
        sources: Vec::new(),
    }
}

fn verdict(overall: ClauseVerdict) -> Verdict {
    Verdict {
        contract_id: "m3-fixture".into(),
        title: "memory verification".into(),
        overall: overall.clone(),
        results: vec![ClauseResult {
            kind: "review".into(),
            desc: "fixture".into(),
            verdict: overall,
            evidence: "fixture".into(),
        }],
        executor_summary: "fixture".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

fn registry(
    memory: SqliteMemoryStore,
    verification: VerificationPolicy,
    reviewer: ReviewProvider,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_memory_with_promotion(
        Arc::new(memory),
        MemoryConfig {
            verification,
            ..MemoryConfig::default()
        },
        vec![Arc::new(reviewer)],
    );
    registry
}

fn signed_registry(
    durable: &Store,
    memory: SqliteMemoryStore,
    verification: VerificationPolicy,
    reviewer: ReviewProvider,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_memory_with_signed_promotion(
        Arc::new(memory),
        MemoryConfig {
            verification,
            ..MemoryConfig::default()
        },
        vec![Arc::new(reviewer)],
        durable.clone(),
    );
    registry
}

#[test]
fn memory_config_resolves_global_agent_contract_layers() {
    let global = MemoryConfigLayer {
        verification: Some(VerificationPolicy::Off),
        identity: Some(IdentityPolicy::Locked),
        extraction: Some(LessonExtractionPolicy::Off),
        lesson_recall_max_lessons: Some(8),
        lesson_recall_token_budget: Some(900),
        playbook_recall_max_playbooks: Some(3),
        playbook_recall_token_budget: Some(700),
        ..MemoryConfigLayer::default()
    };
    let agent = MemoryConfigLayer {
        verification: Some(VerificationPolicy::Lazy),
        identity: None,
        extraction: Some(LessonExtractionPolicy::On),
        ..MemoryConfigLayer::default()
    };
    let contract = MemoryConfigLayer {
        verification: Some(VerificationPolicy::Eager),
        identity: Some(IdentityPolicy::SelfAmend),
        extraction: Some(LessonExtractionPolicy::ModelScored),
        extraction_min_salience: Some(0.85),
        lesson_recall_max_lessons: Some(2),
        playbook_recall_max_playbooks: Some(1),
        ..MemoryConfigLayer::default()
    };

    let resolved = MemoryConfig::resolve(Some(&global), Some(&agent), Some(&contract));
    assert_eq!(resolved.verification, VerificationPolicy::Eager);
    assert_eq!(resolved.identity, IdentityPolicy::SelfAmend);
    assert_eq!(resolved.extraction, LessonExtractionPolicy::ModelScored);
    assert_eq!(resolved.extraction_min_salience, 0.85);
    assert_eq!(resolved.lesson_recall_max_lessons, 2);
    assert_eq!(resolved.lesson_recall_token_budget, 900);
    assert_eq!(resolved.playbook_recall_max_playbooks, 1);
    assert_eq!(resolved.playbook_recall_token_budget, 700);

    let inherited = MemoryConfig::resolve(Some(&global), Some(&agent), None);
    assert_eq!(inherited.verification, VerificationPolicy::Lazy);
    assert_eq!(inherited.identity, IdentityPolicy::Locked);
    assert_eq!(inherited.extraction, LessonExtractionPolicy::On);
    assert_eq!(inherited.playbook_recall_max_playbooks, 3);
}

#[test]
fn effective_budget_policy_resolves_override_ratio_and_layers() {
    let mut global_overrides = BTreeMap::new();
    global_overrides.insert("claude-opus-4-8".to_string(), 250_000);
    let global = MemoryConfigLayer {
        effective_budget: Some(EffectiveBudgetPolicyLayer {
            ratio: Some(0.50),
            overrides: Some(global_overrides),
        }),
        ..MemoryConfigLayer::default()
    };

    let mut agent_overrides = BTreeMap::new();
    agent_overrides.insert("gpt-5".to_string(), 120_000);
    let agent = MemoryConfigLayer {
        effective_budget: Some(EffectiveBudgetPolicyLayer {
            ratio: Some(0.55),
            overrides: Some(agent_overrides),
        }),
        ..MemoryConfigLayer::default()
    };

    let mut contract_overrides = BTreeMap::new();
    contract_overrides.insert("gpt-5".to_string(), 96_000);
    let contract = MemoryConfigLayer {
        effective_budget: Some(EffectiveBudgetPolicyLayer {
            ratio: Some(0.40),
            overrides: Some(contract_overrides),
        }),
        ..MemoryConfigLayer::default()
    };

    let resolved = MemoryConfig::resolve(Some(&global), Some(&agent), Some(&contract));

    assert_eq!(resolved.effective_budget.ratio, 0.40);
    assert_eq!(
        resolved
            .effective_budget
            .resolve(Some("claude-opus-4-8"), 1_000_000),
        250_000
    );
    assert_eq!(
        resolved.effective_budget.resolve(Some("gpt-5"), 256_000),
        96_000
    );
    assert_eq!(
        resolved
            .effective_budget
            .resolve(Some("unknown-model"), 100_000),
        40_000
    );

    let provider = ReviewProvider {
        met: true,
        calls: Arc::new(AtomicUsize::new(0)),
    };
    assert!(provider.effective_context_budget() <= provider.context_window(None) as usize);
}

#[tokio::test]
async fn eager_pass_promotes_and_fail_rejects_never_surfaces() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let ctx = Ctx::new();

    let pass_tools = registry(
        memory.clone(),
        VerificationPolicy::Eager,
        ReviewProvider {
            met: true,
            calls: calls.clone(),
        },
    );
    pass_tools
        .call_with_ledger(
            &ctx,
            "memory",
            json!({
                "command": "create",
                "path": "/memories/facts/build",
                "content": "The build passes."
            }),
            Some(produced(1)),
        )
        .await
        .unwrap();
    let promoted = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Fact),
            key: Some("build".into()),
            min_trust: Trust::Verified,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].trust, Trust::Verified);

    let fail_tools = registry(
        memory.clone(),
        VerificationPolicy::Eager,
        ReviewProvider {
            met: false,
            calls: calls.clone(),
        },
    );
    fail_tools
        .call_with_ledger(
            &ctx,
            "memory",
            json!({
                "command": "create",
                "path": "/memories/facts/deploy",
                "content": "Deploy succeeded."
            }),
            Some(produced(2)),
        )
        .await
        .unwrap();
    let rejected = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Fact),
            key: Some("deploy".into()),
            min_trust: Trust::Unverified,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert!(rejected.is_empty(), "rejected memories never surface");

    let view = fail_tools
        .call(
            &ctx,
            "memory",
            json!({ "command": "view", "path": "/facts" }),
        )
        .await
        .unwrap();
    assert!(view.contains("The build passes."), "{view}");
    assert!(!view.contains("Deploy succeeded."), "{view}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn promotion_records_signed_memory_verdict_in_journal() {
    let durable = Store::open_memory().unwrap();
    durable
        .create_session("m3", Some("fixture"), "local", "local")
        .unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let tools = signed_registry(
        &durable,
        memory,
        VerificationPolicy::Eager,
        ReviewProvider {
            met: true,
            calls: Arc::new(AtomicUsize::new(0)),
        },
    );

    tools
        .call_with_ledger(
            &Ctx::new(),
            "memory",
            json!({
                "command": "create",
                "path": "/memories/facts/signed",
                "content": "This verdict is journaled."
            }),
            Some(produced(1)),
        )
        .await
        .unwrap();

    let entries = durable.entries("m3").unwrap();
    let verdict_entry = entries
        .iter()
        .find(|entry| entry.kind == "memory_verdict")
        .expect("memory verdict journal entry");
    assert!(!verdict_entry.signature.is_empty());
    assert_eq!(verdict_entry.payload["overall"], "Pass");
}

#[tokio::test]
async fn lazy_verifies_once_on_first_recall_then_caches() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    memory
        .write(draft(
            EntryKind::Lesson,
            Some("fmt"),
            "Run cargo fmt before review.",
            1,
        ))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = registry(
        memory,
        VerificationPolicy::Lazy,
        ReviewProvider {
            met: true,
            calls: calls.clone(),
        },
    );
    let ctx = Ctx::new();

    let first = tools
        .call(
            &ctx,
            "memory",
            json!({ "command": "view", "path": "/lessons" }),
        )
        .await
        .unwrap();
    assert!(first.contains("Run cargo fmt before review."), "{first}");
    assert!(first.contains("trust: verified"), "{first}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second = tools
        .call(
            &ctx,
            "memory",
            json!({ "command": "view", "path": "/lessons" }),
        )
        .await
        .unwrap();
    assert!(second.contains("Run cargo fmt before review."), "{second}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "verified verdict is cached in the trust tier"
    );
}

#[tokio::test]
async fn off_surfaces_unverified_tagged_never_as_verified() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = registry(
        memory,
        VerificationPolicy::Off,
        ReviewProvider {
            met: true,
            calls: calls.clone(),
        },
    );
    let ctx = Ctx::new();
    tools
        .call_with_ledger(
            &ctx,
            "memory",
            json!({
                "command": "create",
                "path": "/memories/lessons/off",
                "content": "This stays tagged."
            }),
            Some(produced(1)),
        )
        .await
        .unwrap();

    let view = tools
        .call(
            &ctx,
            "memory",
            json!({ "command": "view", "path": "/lessons" }),
        )
        .await
        .unwrap();
    assert!(view.contains("This stays tagged."), "{view}");
    assert!(view.contains("trust: unverified"), "{view}");
    assert!(!view.contains("trust: verified"), "{view}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tiered_promotes_fact_on_write_and_tags_lower_stakes_entries() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = registry(
        memory.clone(),
        VerificationPolicy::Tiered,
        ReviewProvider {
            met: true,
            calls: calls.clone(),
        },
    );
    let ctx = Ctx::new();

    tools
        .call_with_ledger(
            &ctx,
            "memory",
            json!({
                "command": "create",
                "path": "/memories/facts/rust",
                "content": "Rust is the default language for new core code."
            }),
            Some(produced(1)),
        )
        .await
        .unwrap();
    tools
        .call_with_ledger(
            &ctx,
            "memory",
            json!({
                "command": "create",
                "path": "/memories/lessons/fmt",
                "content": "Run cargo fmt before review."
            }),
            Some(produced(2)),
        )
        .await
        .unwrap();

    let fact = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Fact),
            key: Some("rust".into()),
            min_trust: Trust::Verified,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(fact.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let view = tools
        .call(
            &ctx,
            "memory",
            json!({ "command": "view", "path": "/lessons" }),
        )
        .await
        .unwrap();
    assert!(view.contains("Run cargo fmt before review."), "{view}");
    assert!(view.contains("trust: unverified"), "{view}");
}

#[test]
fn verified_supersede_demotes_prior_to_stale() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let old = memory
        .write(draft(EntryKind::Fact, Some("rust"), "Use Rust.", 1))
        .unwrap();
    memory.promote(&old, verdict(ClauseVerdict::Pass)).unwrap();

    let new = memory
        .write(draft(
            EntryKind::Fact,
            Some("rust"),
            "Use Rust for new core code.",
            2,
        ))
        .unwrap();
    memory.promote(&new, verdict(ClauseVerdict::Pass)).unwrap();

    let old_entry = memory.get(&old).unwrap();
    assert_eq!(old_entry.trust, Trust::Stale);
    assert_eq!(old_entry.superseded_by, Some(new));
}

#[test]
fn verified_lesson_supersede_demotes_prior_to_stale_reversibly() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let old = memory
        .write(draft(
            EntryKind::Lesson,
            Some("fmt"),
            "Run cargo fmt eventually.",
            1,
        ))
        .unwrap();
    memory.promote(&old, verdict(ClauseVerdict::Pass)).unwrap();

    let new = memory
        .write(draft(
            EntryKind::Lesson,
            Some("fmt"),
            "Run cargo fmt before opening a PR.",
            2,
        ))
        .unwrap();
    assert_eq!(memory.get(&old).unwrap().trust, Trust::Verified);
    assert_eq!(memory.get(&old).unwrap().superseded_by, Some(new.clone()));

    memory.promote(&new, verdict(ClauseVerdict::Pass)).unwrap();

    let old_entry = memory.get(&old).unwrap();
    assert_eq!(old_entry.trust, Trust::Stale);
    assert_eq!(old_entry.superseded_by, Some(new.clone()));
    let trusted = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            key: Some("fmt".into()),
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0].id, new);
}
