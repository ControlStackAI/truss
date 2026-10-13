use serde_json::json;

use truss_core::ledger::LedgerReader;
use truss_core::store::{HumanReviewDecision, HumanReviewEntry, Store, HUMAN_REVIEW_KIND};

fn seed_session(store: &Store, session: &str) -> String {
    store
        .create_session(session, Some("human-review-test"), "local", "local")
        .unwrap();
    store
        .append(
            session,
            "verdict",
            &json!({ "overall": "Pass", "clauses": [] }),
        )
        .unwrap();
    store.head_hash_hex(session).unwrap().unwrap()
}

#[test]
fn record_human_review_decisions_are_signed_chained_and_verifiable() {
    for (session, decision) in [
        ("approve-run", HumanReviewDecision::Approved),
        ("reject-run", HumanReviewDecision::Rejected),
        ("needs-changes-run", HumanReviewDecision::NeedsChanges),
    ] {
        let store = Store::open_memory().unwrap();
        let attested = seed_session(&store, session);
        let entry = store
            .record_human_review(session, decision, Some("reviewed evidence".into()))
            .unwrap();
        let payload: HumanReviewEntry = serde_json::from_value(entry.payload.clone()).unwrap();

        assert_eq!(entry.kind, HUMAN_REVIEW_KIND);
        assert_eq!(entry.seq, 1);
        assert!(!entry.signature.is_empty());
        assert_eq!(payload.decision, decision);
        assert_eq!(payload.note.as_deref(), Some("reviewed evidence"));
        assert_eq!(payload.attested_head_hash, attested);
        assert!(!payload.reviewer_key_id.is_empty());
        assert_eq!(store.verify_chain(session).unwrap(), Ok(2));
    }
}

#[test]
fn human_review_signoff_detects_approve_then_mutate() {
    let store = Store::open_memory().unwrap();
    let attested = seed_session(&store, "mutate-run");
    store
        .record_human_review("mutate-run", HumanReviewDecision::Approved, None)
        .unwrap();

    let before_mutation = LedgerReader::new(store.clone())
        .verdict("mutate-run")
        .unwrap()
        .human_signoff
        .expect("human signoff");
    assert_eq!(before_mutation.attested_head_hash, attested);
    assert!(before_mutation.attested_head_matches_current);

    store
        .append(
            "mutate-run",
            "model_result",
            &json!({ "content": "changed after approval" }),
        )
        .unwrap();

    let after_mutation = LedgerReader::new(store.clone())
        .verdict("mutate-run")
        .unwrap()
        .human_signoff
        .expect("human signoff");
    assert_eq!(after_mutation.attested_head_hash, attested);
    assert!(!after_mutation.attested_head_matches_current);
    assert_ne!(
        after_mutation.current_reviewable_head_hash.as_deref(),
        Some(attested.as_str())
    );
}

#[test]
fn verdict_bundle_json_includes_human_signoff_fields() {
    let store = Store::open_memory().unwrap();
    let attested = seed_session(&store, "json-run");
    store
        .record_human_review(
            "json-run",
            HumanReviewDecision::NeedsChanges,
            Some("missing artifact".into()),
        )
        .unwrap();

    let bundle = LedgerReader::new(store).verdict("json-run").unwrap();
    let value = serde_json::to_value(bundle).unwrap();
    let signoff = &value["human_signoff"];

    assert_eq!(signoff["decision"], "needs_changes");
    assert_eq!(signoff["note"], "missing artifact");
    assert_eq!(signoff["attested_head_hash"], attested);
    assert!(signoff["reviewer_key_id"].as_str().unwrap().len() >= 64);
    assert_eq!(signoff["attested_head_matches_current"], true);
    assert_eq!(signoff["verification_status"], "verified");
}
