use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use truss_core::contract::{BudgetSpec, ConstraintSpec, Contract, ExecutorSpec, Meta};
use truss_core::ctx::Ctx;
use truss_core::delegation::{self, ClauseVerdict, ContractCloseLearning};
use truss_core::durable::DurableConfig;
use truss_core::error::ProviderError;
use truss_core::events::AgentEvent;
use truss_core::executor::Executor;
use truss_core::memory::{
    EntryKind, LessonExtractionPolicy, MemoryConfig, MemoryCursor, MemoryQuery, MemoryStore,
    SqliteMemoryStore, Trust, VerificationPolicy,
};
use truss_core::message::Block;
use truss_core::provider::{CompletionRequest, CompletionResponse, Provider, StopReason, Usage};
use truss_core::store::Store;
use truss_core::tools::ToolRegistry;

struct ScriptedProvider {
    calls: Arc<AtomicUsize>,
    responses: Vec<CompletionResponse>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn default_model(&self) -> &str {
        "scripted-model"
    }

    async fn complete(
        &self,
        _ctx: &Ctx,
        _req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(i)
            .cloned()
            .ok_or_else(|| ProviderError::Config(format!("script exhausted at call {i}")))
    }
}

fn text(body: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![Block::Text(body.into())],
        stop_reason: StopReason::EndTurn,
        model: "scripted-model".into(),
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
    }
}

fn review(met: Option<bool>) -> CompletionResponse {
    match met {
        Some(met) => text(&format!(
            r#"{{"met": {met}, "reasoning": "scripted panel vote"}}"#
        )),
        None => text("not-json"),
    }
}

fn contract(id: &str) -> Contract {
    Contract {
        contract: Meta {
            id: id.into(),
            title: "Extract lessons".into(),
            intent: "Complete a small verified run so durable lessons can be proposed.".into(),
        },
        done: Vec::new(),
        budget: BudgetSpec::default(),
        constraints: ConstraintSpec::default(),
        reviewers: None,
        memory: Default::default(),
        sandbox: Default::default(),
        approval_gates: Default::default(),
        mcp: Default::default(),
        executor: ExecutorSpec::default(),
    }
}

#[tokio::test]
async fn extraction_on_writes_unverified_lesson_with_verdict_provenance() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    let extractor_calls = Arc::new(AtomicUsize::new(0));
    let extractor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: extractor_calls.clone(),
        responses: vec![text(
            r#"{"lessons":[{"key":"fmt-before-pr","body":"Run cargo fmt before opening a PR.","salience":0.91}]}"#,
        )],
    });
    let executor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        responses: vec![text("completed")],
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Ctx::new().with_events(tx);
    let cfg = DurableConfig {
        memory: MemoryConfig {
            extraction: LessonExtractionPolicy::On,
            ..MemoryConfig::default()
        },
        ..DurableConfig::default()
    };

    let verdict = delegation::run_contract_with_close_learning(
        Executor::InLoop(executor),
        Vec::new(),
        Arc::new(ToolRegistry::new()),
        &ctx,
        &durable,
        &contract("lesson-on"),
        &cfg,
        Some(ContractCloseLearning {
            memory_store: memory.clone(),
            extractor,
        }),
    )
    .await
    .unwrap();
    assert_eq!(verdict.overall, ClauseVerdict::Pass);
    assert_eq!(extractor_calls.load(Ordering::SeqCst), 1);

    let lessons = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            min_trust: Trust::Unverified,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(lessons.len(), 1);
    let lesson = &lessons[0];
    assert_eq!(lesson.key.as_deref(), Some("fmt-before-pr"));
    assert_eq!(lesson.trust, Trust::Unverified);
    assert_eq!(lesson.provenance.produced_by.session, "lesson-on");
    let verdict_seq = durable
        .entries("lesson-on")
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "verdict")
        .expect("verdict entry")
        .seq;
    assert_eq!(lesson.provenance.produced_by.seq, verdict_seq);
    assert_eq!(
        lesson.provenance.sources,
        vec![lesson.provenance.produced_by.clone()]
    );

    let event = std::iter::from_fn(|| rx.try_recv().ok())
        .find(|event| matches!(event, AgentEvent::MemoryWrite { .. }))
        .expect("memory write event");
    match event {
        AgentEvent::MemoryWrite {
            kind,
            key,
            trust,
            produced_by,
            ..
        } => {
            assert_eq!(kind, EntryKind::Lesson);
            assert_eq!(key.as_deref(), Some("fmt-before-pr"));
            assert_eq!(trust, Trust::Unverified);
            assert_eq!(produced_by, lesson.provenance.produced_by);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn eager_extraction_promotes_lesson_after_passing_panel_verdict() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    let extractor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        responses: vec![text(
            r#"{"lessons":[{"key":"fmt-before-pr","body":"Run cargo fmt before opening a PR.","salience":0.91}]}"#,
        )],
    });
    let executor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        responses: vec![text("completed")],
    });
    let reviewer_calls = Arc::new(AtomicUsize::new(0));
    let reviewer: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: reviewer_calls.clone(),
        responses: vec![review(Some(true))],
    });
    let cfg = DurableConfig {
        memory: MemoryConfig {
            extraction: LessonExtractionPolicy::On,
            verification: VerificationPolicy::Eager,
            ..MemoryConfig::default()
        },
        ..DurableConfig::default()
    };

    delegation::run_contract_with_close_learning(
        Executor::InLoop(executor),
        vec![reviewer],
        Arc::new(ToolRegistry::new()),
        &Ctx::new(),
        &durable,
        &contract("lesson-eager-pass"),
        &cfg,
        Some(ContractCloseLearning {
            memory_store: memory.clone(),
            extractor,
        }),
    )
    .await
    .unwrap();

    let lessons = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            key: Some("fmt-before-pr".into()),
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(lessons.len(), 1);
    let lesson = &lessons[0];
    assert_eq!(lesson.trust, Trust::Verified);
    assert_eq!(
        lesson.provenance.verified_by.as_ref().unwrap().overall,
        ClauseVerdict::Pass
    );
    assert_eq!(reviewer_calls.load(Ordering::SeqCst), 1);

    let entries = durable.entries("lesson-eager-pass").unwrap();
    let memory_verdict = entries
        .iter()
        .find(|entry| entry.kind == "memory_verdict")
        .expect("signed memory verdict");
    assert!(!memory_verdict.signature.is_empty());
    assert_eq!(memory_verdict.payload["overall"], "Pass");
    assert!(durable.verify_chain("lesson-eager-pass").unwrap().is_ok());
}

#[tokio::test]
async fn eager_extraction_rejects_lesson_after_failing_or_unverifiable_panel() {
    for (case, review_response, expected) in [
        (
            "lesson-eager-fail",
            review(Some(false)),
            ClauseVerdict::Fail,
        ),
        (
            "lesson-eager-unverifiable",
            review(None),
            ClauseVerdict::Unverifiable,
        ),
    ] {
        let durable = Store::open_memory().unwrap();
        let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
        let extractor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            responses: vec![text(
                r#"{"lessons":[{"key":"bad-claim","body":"Trust every generated artifact without checking it.","salience":0.91}]}"#,
            )],
        });
        let executor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            responses: vec![text("completed")],
        });
        let reviewer: Arc<dyn Provider> = Arc::new(ScriptedProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            responses: vec![review_response.clone()],
        });
        let cfg = DurableConfig {
            memory: MemoryConfig {
                extraction: LessonExtractionPolicy::On,
                verification: VerificationPolicy::Eager,
                ..MemoryConfig::default()
            },
            ..DurableConfig::default()
        };

        delegation::run_contract_with_close_learning(
            Executor::InLoop(executor),
            vec![reviewer],
            Arc::new(ToolRegistry::new()),
            &Ctx::new(),
            &durable,
            &contract(case),
            &cfg,
            Some(ContractCloseLearning {
                memory_store: memory.clone(),
                extractor,
            }),
        )
        .await
        .unwrap();

        assert!(memory
            .query(&MemoryQuery {
                kind: Some(EntryKind::Lesson),
                ..MemoryQuery::default()
            })
            .unwrap()
            .is_empty());
        let delta = memory.delta_since(MemoryCursor(0)).unwrap();
        let rejected = delta
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::Lesson)
            .expect("rejected lesson remains auditable");
        assert_eq!(rejected.trust, Trust::Rejected);
        assert!(rejected.provenance.verified_by.is_none());

        let memory_verdict = durable
            .entries(case)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "memory_verdict")
            .expect("signed memory verdict");
        assert_eq!(memory_verdict.payload["overall"], format!("{expected:?}"));
        assert!(durable.verify_chain(case).unwrap().is_ok());
    }
}

#[tokio::test]
async fn verification_off_extracts_but_never_auto_promotes_lesson() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    let extractor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        responses: vec![text(
            r#"{"lessons":[{"key":"fmt-before-pr","body":"Run cargo fmt before opening a PR.","salience":0.91}]}"#,
        )],
    });
    let executor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        responses: vec![text("completed")],
    });
    let reviewer_calls = Arc::new(AtomicUsize::new(0));
    let reviewer: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: reviewer_calls.clone(),
        responses: vec![review(Some(true))],
    });
    let cfg = DurableConfig {
        memory: MemoryConfig {
            extraction: LessonExtractionPolicy::On,
            verification: VerificationPolicy::Off,
            ..MemoryConfig::default()
        },
        ..DurableConfig::default()
    };

    delegation::run_contract_with_close_learning(
        Executor::InLoop(executor),
        vec![reviewer],
        Arc::new(ToolRegistry::new()),
        &Ctx::new(),
        &durable,
        &contract("lesson-off-promotion"),
        &cfg,
        Some(ContractCloseLearning {
            memory_store: memory.clone(),
            extractor,
        }),
    )
    .await
    .unwrap();

    assert!(memory.query(&MemoryQuery::default()).unwrap().is_empty());
    let unverified = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            min_trust: Trust::Unverified,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(unverified.len(), 1);
    assert_eq!(unverified[0].trust, Trust::Unverified);
    assert!(unverified[0].provenance.verified_by.is_none());
    assert_eq!(reviewer_calls.load(Ordering::SeqCst), 0);
    assert!(!durable
        .entries("lesson-off-promotion")
        .unwrap()
        .iter()
        .any(|entry| entry.kind == "memory_verdict"));
}

#[tokio::test]
async fn extraction_off_writes_nothing_and_makes_no_extractor_call() {
    let durable = Store::open_memory().unwrap();
    let memory = Arc::new(SqliteMemoryStore::from_store(&durable).unwrap());
    let extractor_calls = Arc::new(AtomicUsize::new(0));
    let extractor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: extractor_calls.clone(),
        responses: Vec::new(),
    });
    let executor: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        calls: Arc::new(AtomicUsize::new(0)),
        responses: vec![text("completed")],
    });
    let cfg = DurableConfig {
        memory: MemoryConfig {
            extraction: LessonExtractionPolicy::Off,
            ..MemoryConfig::default()
        },
        ..DurableConfig::default()
    };

    delegation::run_contract_with_close_learning(
        Executor::InLoop(executor),
        Vec::new(),
        Arc::new(ToolRegistry::new()),
        &Ctx::new(),
        &durable,
        &contract("lesson-off"),
        &cfg,
        Some(ContractCloseLearning {
            memory_store: memory.clone(),
            extractor,
        }),
    )
    .await
    .unwrap();

    assert_eq!(extractor_calls.load(Ordering::SeqCst), 0);
    assert!(memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            min_trust: Trust::Unverified,
            ..MemoryQuery::default()
        })
        .unwrap()
        .is_empty());
}
