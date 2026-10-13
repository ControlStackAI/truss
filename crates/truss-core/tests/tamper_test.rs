use serde_json::json;

use truss_core::budget::{BudgetLimits, BudgetSnapshot};
use truss_core::child_run::{run_child, ChildRunOptions, ChildRunOutput};
use truss_core::ctx::Ctx;
use truss_core::delegation::{apply_memory_verdict, ClauseResult, ClauseVerdict, Verdict};
use truss_core::events::ArtifactRef;
use truss_core::memory::{
    EntryKind, LedgerRef, MemoryDraft, MemoryQuery, MemoryStore, SqliteMemoryStore, Trust,
};
use truss_core::store::{HumanReviewDecision, Store, CHILD_ANCHOR_KIND, HUMAN_REVIEW_KIND};

fn append_valid_chain(store: &Store, session: &str) {
    store
        .create_session(session, Some("tamper-test"), "local", "local")
        .unwrap();
    for seq in 0..3 {
        store
            .append(
                session,
                "event",
                &json!({ "seq": seq, "message": format!("entry-{seq}") }),
            )
            .unwrap();
    }
    assert_eq!(store.verify_chain(session).unwrap(), Ok(3));
}

fn pass_verdict(id: &str) -> Verdict {
    Verdict {
        contract_id: id.into(),
        title: "panel verdict".into(),
        overall: ClauseVerdict::Pass,
        results: vec![ClauseResult {
            kind: "review".into(),
            desc: "claim is supported".into(),
            verdict: ClauseVerdict::Pass,
            evidence: "fixture panel passed".into(),
        }],
        executor_summary: "verified fixture output".into(),
        reviewed_diff: None,
        usage: BudgetSnapshot::default(),
    }
}

fn child_options(task: &str) -> ChildRunOptions {
    ChildRunOptions::new(
        task,
        BudgetLimits {
            max_steps: Some(5),
            ..Default::default()
        },
    )
}

fn artifact(name: &str) -> ArtifactRef {
    ArtifactRef {
        path: format!("out/{name}.txt"),
        kind: "text/plain".into(),
        sha256: format!("sha-{name}"),
        size_bytes: 8,
    }
}

#[test]
fn untampered_chain_passes_control() {
    let store = Store::open_memory().unwrap();
    append_valid_chain(&store, "control");

    assert!(store.verify_chain("control").unwrap().is_ok());
}

#[test]
fn corrupt_single_entry_stored_hash_fails_verify() {
    let store = Store::open_memory().unwrap();
    append_valid_chain(&store, "corrupt-hash");

    store
        .raw_corrupt_entry_hash_for_test("corrupt-hash", 1)
        .unwrap();

    assert!(store.verify_chain("corrupt-hash").unwrap().is_err());
}

#[test]
fn flipped_signature_byte_fails_verify() {
    let store = Store::open_memory().unwrap();
    append_valid_chain(&store, "bad-signature");

    store
        .raw_flip_signature_byte_for_test("bad-signature", 1)
        .unwrap();

    assert!(store.verify_chain("bad-signature").unwrap().is_err());
}

#[test]
fn reordered_ledger_entries_fail_verify() {
    let store = Store::open_memory().unwrap();
    append_valid_chain(&store, "reordered");

    store
        .raw_swap_entry_order_for_test("reordered", 1, 2)
        .unwrap();

    assert!(store.verify_chain("reordered").unwrap().is_err());
}

#[test]
fn replayed_duplicate_entry_fails_verify() {
    let store = Store::open_memory().unwrap();
    append_valid_chain(&store, "replayed");

    store.raw_duplicate_entry_for_test("replayed", 1).unwrap();

    assert!(store.verify_chain("replayed").unwrap().is_err());
}

#[test]
fn tampered_human_review_payload_fails_verify() {
    let store = Store::open_memory().unwrap();
    store
        .create_session(
            "human-review-payload",
            Some("tamper-test"),
            "local",
            "local",
        )
        .unwrap();
    store
        .append(
            "human-review-payload",
            "verdict",
            &json!({ "overall": "Pass", "clauses": [] }),
        )
        .unwrap();
    let entry = store
        .record_human_review(
            "human-review-payload",
            HumanReviewDecision::Approved,
            Some("looks good".into()),
        )
        .unwrap();
    assert_eq!(entry.kind, HUMAN_REVIEW_KIND);

    let mut payload = entry.payload;
    payload["decision"] = json!("rejected");
    store
        .raw_set_payload_for_test("human-review-payload", entry.seq, &payload)
        .unwrap();

    assert!(store.verify_chain("human-review-payload").unwrap().is_err());
}

#[test]
fn tampered_human_review_signature_fails_verify() {
    let store = Store::open_memory().unwrap();
    store
        .create_session("human-review-sig", Some("tamper-test"), "local", "local")
        .unwrap();
    store
        .append(
            "human-review-sig",
            "verdict",
            &json!({ "overall": "Pass", "clauses": [] }),
        )
        .unwrap();
    let entry = store
        .record_human_review("human-review-sig", HumanReviewDecision::Rejected, None)
        .unwrap();
    assert_eq!(entry.kind, HUMAN_REVIEW_KIND);

    store
        .raw_flip_signature_byte_for_test("human-review-sig", entry.seq)
        .unwrap();

    assert!(store.verify_chain("human-review-sig").unwrap().is_err());
}

#[test]
fn truncated_chain_dropping_head_is_detected() {
    let store = Store::open_memory().unwrap();
    append_valid_chain(&store, "truncated");

    store.raw_delete_entry_for_test("truncated", 2).unwrap();

    assert!(
        store.verify_chain("truncated").unwrap().is_err(),
        "session head hash must detect a missing tail entry"
    );
}

#[test]
fn forged_verdict_signature_mismatch_rejects_chain() {
    let durable = Store::open_memory().unwrap();
    durable
        .create_session("verdict-forge", Some("memory"), "local", "local")
        .unwrap();
    let produced = durable
        .append("verdict-forge", "memory_write", &json!({ "claim": "x" }))
        .unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let id = memory
        .write(MemoryDraft {
            kind: EntryKind::Lesson,
            key: Some("signed-verdict".into()),
            body: "A verified claim must have a signed verdict.".into(),
            produced_by: LedgerRef {
                session: "verdict-forge".into(),
                seq: produced.seq,
            },
            sources: vec![LedgerRef {
                session: "verdict-forge".into(),
                seq: produced.seq,
            }],
        })
        .unwrap();

    apply_memory_verdict(&memory, Some(&durable), &id, pass_verdict("memory-panel")).unwrap();
    let verified = memory.query(&MemoryQuery::default()).unwrap();
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].trust, Trust::Verified);
    assert!(verified[0].provenance.verified_by_ref.is_some());
    assert!(durable.verify_chain("verdict-forge").unwrap().is_ok());

    let verdict_ref = verified[0].provenance.verified_by_ref.as_ref().unwrap();
    durable
        .raw_flip_signature_byte_for_test(&verdict_ref.session, verdict_ref.seq)
        .unwrap();

    assert!(durable.verify_chain("verdict-forge").unwrap().is_err());
}

#[tokio::test]
async fn swapped_child_anchor_head_fails_verify_even_when_rechained() {
    let store = Store::open_memory().unwrap();
    let parent = Ctx::new();

    let first = run_child(
        &parent,
        &store,
        child_options("first child"),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: pass_verdict("first-child"),
                artifacts: vec![artifact("first")],
            })
        },
    )
    .await
    .unwrap();
    let second = run_child(
        &parent,
        &store,
        child_options("second child"),
        |child_ctx, _store| async move {
            child_ctx.budget.record_step();
            Ok(ChildRunOutput {
                verdict: pass_verdict("second-child"),
                artifacts: vec![artifact("second")],
            })
        },
    )
    .await
    .unwrap();

    assert!(store.verify_chain(&parent.run_id).unwrap().is_ok());
    let mut anchor_payload = store
        .entries(&parent.run_id)
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == CHILD_ANCHOR_KIND && entry.seq == first.parent_anchor.seq)
        .expect("first child anchor")
        .payload;
    anchor_payload["child_head_hash"] = json!(second.child_head_hash);

    store
        .raw_replace_payload_and_rechain_for_test(
            &parent.run_id,
            first.parent_anchor.seq,
            &anchor_payload,
        )
        .unwrap();

    assert!(
        store.verify_chain(&parent.run_id).unwrap().is_err(),
        "anchor semantic verification must reject a repointed child head"
    );
}
