use truss_core::identity::Signer;
use truss_core::memory::{
    Amendment, AmendmentDecision, AmendmentTarget, ConflictResolution, ContextAssembler, EntryKind,
    GoalLedger, IdentityPolicy, LedgerRef, MemoryConfig, MemoryConfigLayer, MemoryCursor,
    MemoryDraft, MemoryScope, MemoryStore, Persona, PersonaCore, PersonaSelf, ProjectionConfig,
    SaliencePolicy, SqliteMemoryStore, VerificationPolicy,
};
use truss_core::message::Message;
use truss_core::message::Role;

fn ledger(seq: i64) -> LedgerRef {
    LedgerRef {
        session: "m4".into(),
        seq,
    }
}

fn persona(signer: &Signer) -> Persona {
    Persona {
        core: PersonaCore::signed("L0 core: protect boundaries.", signer),
        self_: PersonaSelf {
            body: "L0 self: concise builder.".into(),
            last_amended_by: None,
            version: 1,
        },
    }
}

#[test]
fn assembled_prompt_keeps_stable_l0_l1_prefix_when_l3_l4_change() {
    let signer = Signer::generate();
    let persona = persona(&signer);
    let goal = GoalLedger::from_contract("Ship M4", vec!["tests pass".into()], 8);

    let first = ContextAssembler::new(
        Some(persona.clone()),
        Some("L1 principal: the operator prefers concise reports.".into()),
        Some("extra stable system".into()),
    )
    .goal_ledger(Some(goal.clone()))
    .history(vec![Message::user("turn 1")])
    .scratch(Some("working note A".into()))
    .step(1)
    .assemble();

    let second = ContextAssembler::new(
        Some(persona),
        Some("L1 principal: the operator prefers concise reports.".into()),
        Some("extra stable system".into()),
    )
    .goal_ledger(Some(goal))
    .history(vec![Message::user("turn 2")])
    .scratch(Some("working note B".into()))
    .step(1)
    .assemble();

    assert_eq!(first.stable_prefix_key, second.stable_prefix_key);
    assert_eq!(first.system, second.system);
    assert!(
        first.system.last().unwrap().cache_control.is_some(),
        "last L0/L1 block is the cache breakpoint"
    );
    assert_ne!(
        first.messages.last().unwrap().text(),
        second.messages.last().unwrap().text()
    );
}

#[test]
fn core_signature_mismatch_refuses_to_load() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "truss-persona-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let signer = Signer::generate();
    let mut p = persona(&signer);
    p.persist(&path).unwrap();
    p.core.body.push_str(" tampered");
    std::fs::write(&path, serde_json::to_string_pretty(&p).unwrap()).unwrap();
    let err = Persona::load(&path).unwrap_err().to_string();
    assert!(err.contains("signature mismatch"), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn amendment_policy_propose_self_amend_rollback_and_core_denied() {
    let signer = Signer::generate();
    let mut p = persona(&signer);
    let amendment = Amendment::for_self("L0 self: precise and direct.", ledger(10), &signer);

    assert_eq!(
        p.route_amendment(&amendment, IdentityPolicy::Propose, true),
        AmendmentDecision::NeedsApproval
    );
    assert_eq!(
        p.route_amendment(&amendment, IdentityPolicy::SelfAmend, true),
        AmendmentDecision::CommitAtBoundary
    );

    let event = p.commit_amendment(&amendment, ledger(11)).unwrap();
    assert_eq!(p.self_.body, "L0 self: precise and direct.");
    assert_eq!(p.self_.version, 2);
    p.rollback(&event);
    assert_eq!(p.self_.body, "L0 self: concise builder.");
    assert_eq!(p.self_.version, 1);

    let mut core_attempt = Amendment::for_self("rewrite core", ledger(12), &signer);
    core_attempt.target = AmendmentTarget::Core;
    core_attempt.signature = hex::encode(signer.sign(core_attempt.signing_payload().as_bytes()));
    assert!(matches!(
        p.route_amendment(&core_attempt, IdentityPolicy::SelfAmend, true),
        AmendmentDecision::Denied(_)
    ));
}

#[test]
fn config_layers_scope_conflict_projection_and_salience() {
    let global = MemoryConfigLayer {
        verification: Some(VerificationPolicy::Off),
        identity: Some(IdentityPolicy::Locked),
        scope: Some(MemoryScope::PerProject),
        conflict: Some(ConflictResolution::LatestWins),
        goal_repin_every: Some(12),
        projection: Some(ProjectionConfig {
            keep_tail: 6,
            ..ProjectionConfig::default()
        }),
        salience: Some(SaliencePolicy::HarnessRecency),
        ..MemoryConfigLayer::default()
    };
    let agent = MemoryConfigLayer {
        scope: Some(MemoryScope::PerAgent),
        conflict: Some(ConflictResolution::VerifiedWins),
        ..MemoryConfigLayer::default()
    };
    let contract = MemoryConfigLayer {
        scope: Some(MemoryScope::Shared),
        goal_repin_every: Some(3),
        ..MemoryConfigLayer::default()
    };

    let resolved = MemoryConfig::resolve(Some(&global), Some(&agent), Some(&contract));
    assert_eq!(resolved.verification, VerificationPolicy::Off);
    assert_eq!(resolved.identity, IdentityPolicy::Locked);
    assert_eq!(resolved.scope, MemoryScope::Shared);
    assert_eq!(resolved.conflict, ConflictResolution::VerifiedWins);
    assert_eq!(resolved.goal_repin_every, 3);
    assert_eq!(resolved.projection.keep_tail, 6);
    assert_eq!(resolved.salience, SaliencePolicy::HarnessRecency);
}

#[test]
fn goal_ledger_repin_keeps_intent_outside_compacted_history() {
    let mut history = vec![Message::user("original task")];
    for i in 0..20 {
        history.push(Message {
            role: Role::Assistant,
            content: vec![truss_core::message::Block::Text(format!("step {i}"))],
        });
    }
    let projected = truss_core::projection::project(&history, 3);
    assert!(projected[1].text().contains("elided"));

    let goal = GoalLedger::from_contract("Do not lose this intent", vec!["finish M4".into()], 8);
    let assembled = ContextAssembler::new(None, None, None)
        .goal_ledger(Some(goal))
        .history(projected)
        .step(8)
        .assemble();

    assert!(assembled.messages[0]
        .text()
        .contains("Do not lose this intent"));
    assert!(assembled.messages[1].text().contains("original task"));
    assert!(assembled.messages[2].text().contains("elided"));
}

#[test]
fn assembler_injects_memory_delta_since_cursor_not_whole_store() {
    let store = SqliteMemoryStore::open_memory().unwrap();
    let old = store
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some("old".into()),
            body: "old lesson".into(),
            produced_by: ledger(1),
            sources: Vec::new(),
        })
        .unwrap();
    let cursor = store.delta_since(MemoryCursor(0)).unwrap().cursor;
    let new = store
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some("new".into()),
            body: "new lesson".into(),
            produced_by: ledger(2),
            sources: Vec::new(),
        })
        .unwrap();

    let delta = store.delta_since(cursor).unwrap();
    assert_eq!(delta.entries.len(), 1);
    assert_eq!(delta.entries[0].id, new);
    assert_ne!(delta.entries[0].id, old);

    let assembled = ContextAssembler::new(None, None, None)
        .memory_delta(Some(delta), cursor)
        .assemble();
    let text = assembled.messages.last().unwrap().text();
    assert!(text.contains("new lesson"), "{text}");
    assert!(!text.contains("old lesson"), "{text}");
    assert!(assembled.next_memory_cursor.0 > cursor.0);
}
