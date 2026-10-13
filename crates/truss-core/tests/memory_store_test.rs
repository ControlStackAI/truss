use truss_core::budget::BudgetSnapshot;
use truss_core::delegation::{ClauseResult, ClauseVerdict, Verdict};
use truss_core::memory::{
    EntryKind, LedgerRef, MemoryCursor, MemoryDraft, MemoryQuery, MemoryStore, Policy, PolicyDraft,
    PolicyTarget, SqliteMemoryStore, Trust,
};
use truss_core::store::Store;

fn store() -> SqliteMemoryStore {
    let durable = Store::open_memory().unwrap();
    SqliteMemoryStore::from_store(&durable).unwrap()
}

fn produced(seq: i64) -> LedgerRef {
    LedgerRef {
        session: "session-1".into(),
        seq,
    }
}

fn draft(kind: EntryKind, key: Option<&str>, body: &str, seq: i64) -> MemoryDraft {
    MemoryDraft {
        kind,
        key: key.map(str::to_string),
        body: body.into(),
        produced_by: produced(seq),
        sources: vec![produced(seq + 100)],
    }
}

fn verdict(overall: ClauseVerdict) -> Verdict {
    Verdict {
        contract_id: "contract-1".into(),
        title: "memory verification".into(),
        overall: overall.clone(),
        results: vec![ClauseResult {
            kind: "check".into(),
            desc: "memory claim is supported".into(),
            verdict: overall,
            evidence: "fixture evidence".into(),
        }],
        executor_summary: "verified by fixture".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

#[test]
fn write_get_round_trip_lands_unverified() {
    let store = store();
    let id = store
        .write(draft(EntryKind::Fact, Some("rust"), "Rust is default.", 1))
        .unwrap();

    let entry = store.get(&id).unwrap();
    assert_eq!(entry.id, id);
    assert_eq!(entry.kind, EntryKind::Fact);
    assert_eq!(entry.key.as_deref(), Some("rust"));
    assert_eq!(entry.body, "Rust is default.");
    assert_eq!(entry.trust, Trust::Unverified);
    assert_eq!(entry.provenance.produced_by, produced(1));
    assert_eq!(entry.provenance.sources, vec![produced(101)]);
    assert!(entry.provenance.verified_by.is_none());
}

#[test]
fn supersede_by_key_links_prior_and_hides_it_from_default_query() {
    let store = store();
    let old = store
        .write(draft(
            EntryKind::Lesson,
            Some("fmt"),
            "Run fmt eventually.",
            1,
        ))
        .unwrap();
    store.promote(&old, verdict(ClauseVerdict::Pass)).unwrap();
    assert_eq!(store.query(&MemoryQuery::default()).unwrap().len(), 1);

    let new = store
        .write(draft(
            EntryKind::Lesson,
            Some("fmt"),
            "Run cargo fmt before every commit.",
            2,
        ))
        .unwrap();

    let old_entry = store.get(&old).unwrap();
    assert_eq!(old_entry.superseded_by, Some(new.clone()));
    assert!(
        store.query(&MemoryQuery::default()).unwrap().is_empty(),
        "superseded verified entry should not be active, and replacement is unverified"
    );
}

#[test]
fn query_default_is_verified_only_and_min_trust_is_a_floor() {
    let store = store();
    let id = store
        .write(draft(
            EntryKind::Fact,
            Some("build"),
            "The build is green.",
            1,
        ))
        .unwrap();

    assert!(store.query(&MemoryQuery::default()).unwrap().is_empty());

    let unverified = store
        .query(&MemoryQuery {
            min_trust: Trust::Unverified,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(unverified.len(), 1);
    assert_eq!(unverified[0].id, id);

    store.promote(&id, verdict(ClauseVerdict::Pass)).unwrap();
    let verified = store.query(&MemoryQuery::default()).unwrap();
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].trust, Trust::Verified);

    store.mark_stale(&id, None).unwrap();
    assert!(
        store.query(&MemoryQuery::default()).unwrap().is_empty(),
        "verified-only default should exclude stale entries"
    );
    let stale_or_better = store
        .query(&MemoryQuery {
            min_trust: Trust::Stale,
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(stale_or_better.len(), 1);
    assert_eq!(stale_or_better[0].trust, Trust::Stale);
}

#[test]
fn unverified_lessons_are_excluded_from_trusted_default_query() {
    let store = store();
    store
        .write(draft(
            EntryKind::Lesson,
            Some("fmt-first"),
            "Run cargo fmt before opening a PR.",
            1,
        ))
        .unwrap();

    let trusted = store
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            ..MemoryQuery::default()
        })
        .unwrap();
    assert!(
        trusted.is_empty(),
        "default trusted view must not surface unverified lessons"
    );
}

#[test]
fn promote_attaches_verdict_flips_tier_and_appears_in_next_delta() {
    let store = store();
    let id = store
        .write(draft(EntryKind::Decision, None, "Use SQLite for M1.", 1))
        .unwrap();
    let after_write = store.delta_since(MemoryCursor(0)).unwrap().cursor;

    store.promote(&id, verdict(ClauseVerdict::Pass)).unwrap();

    let entry = store.get(&id).unwrap();
    assert_eq!(entry.trust, Trust::Verified);
    let stored_verdict = entry.provenance.verified_by.unwrap();
    assert_eq!(stored_verdict.contract_id, "contract-1");
    assert_eq!(stored_verdict.overall, ClauseVerdict::Pass);

    let delta = store.delta_since(after_write).unwrap();
    assert_eq!(delta.entries.len(), 1);
    assert_eq!(delta.entries[0].id, id);
    assert_eq!(delta.entries[0].trust, Trust::Verified);
    assert!(delta.cursor.0 > after_write.0);
}

#[test]
fn governed_policy_record_round_trips_with_signed_provenance() {
    let durable = Store::open_memory().unwrap();
    durable
        .create_session("policy-session", Some("test"), "local", "local")
        .unwrap();
    let policy_event = durable
        .append(
            "policy-session",
            "policy_record",
            &serde_json::json!({
                "key": "memory.verification",
                "schema_version": 1,
            }),
        )
        .unwrap();
    let store = SqliteMemoryStore::from_store(&durable).unwrap();
    let produced_by = LedgerRef {
        session: "policy-session".into(),
        seq: policy_event.seq,
    };

    let policy = Policy {
        schema_version: 1,
        target: PolicyTarget::Verification,
        key: "memory.verification".into(),
        body: serde_json::json!({ "verification": "lazy" }),
    };
    let id = store
        .write_policy(PolicyDraft {
            policy: policy.clone(),
            produced_by: produced_by.clone(),
            sources: vec![produced(42)],
        })
        .unwrap();

    let record = store.get_policy(&id).unwrap();
    assert_eq!(record.id, id);
    assert_eq!(record.policy, policy);
    assert_eq!(record.trust, Trust::Unverified);
    assert_eq!(record.provenance.produced_by, produced_by);
    assert_eq!(record.provenance.sources, vec![produced(42)]);
    assert!(record.provenance.verified_by.is_none());

    let journal_entry = durable
        .entries("policy-session")
        .unwrap()
        .into_iter()
        .find(|entry| entry.seq == policy_event.seq)
        .expect("policy journal event");
    assert!(!journal_entry.signature.is_empty());
    assert!(durable.verify_chain("policy-session").unwrap().is_ok());
}

#[test]
fn delta_since_returns_only_post_cursor_changes_including_tier_updates() {
    let store = store();
    let before = store
        .write(draft(EntryKind::Artifact, None, "artifact://before", 1))
        .unwrap();
    let cursor = store.delta_since(MemoryCursor(0)).unwrap().cursor;

    let after = store
        .write(draft(EntryKind::Artifact, None, "artifact://after", 2))
        .unwrap();
    store.promote(&after, verdict(ClauseVerdict::Pass)).unwrap();

    let delta = store.delta_since(cursor).unwrap();
    assert_eq!(delta.entries.len(), 1);
    assert_eq!(delta.entries[0].id, after);
    assert_eq!(delta.entries[0].trust, Trust::Verified);
    assert!(delta.entries.iter().all(|entry| entry.id != before));
    assert!(delta.cursor.0 > cursor.0);
}
