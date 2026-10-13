use std::sync::Arc;

use truss_core::budget::BudgetSnapshot;
use truss_core::delegation::{ClauseResult, ClauseVerdict, Verdict};
use truss_core::memory::{
    recall_verified_lessons, DeterministicEmbedder, EntryKind, LedgerRef, MemoryConfig,
    MemoryDraft, MemoryQuery, MemoryRecallMode, MemoryStore, MemoryStoreOptions, SqliteMemoryStore,
    Trust,
};
use truss_core::store::Store;

fn produced(seq: i64) -> LedgerRef {
    LedgerRef {
        session: "semantic-test".into(),
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

fn verdict() -> Verdict {
    Verdict {
        contract_id: "semantic-memory".into(),
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

fn semantic_store(durable: &Store) -> SqliteMemoryStore {
    SqliteMemoryStore::from_store_with_options(
        durable,
        MemoryStoreOptions::semantic(Arc::new(DeterministicEmbedder::new(32))),
    )
    .unwrap()
}

#[test]
fn write_persists_embedding_metadata() {
    let durable = Store::open_memory().unwrap();
    let memory = semantic_store(&durable);

    let id = memory
        .write(draft(
            EntryKind::Fact,
            Some("rust"),
            "Rust ownership and borrowing are core language rules.",
            1,
        ))
        .unwrap();

    let metadata = memory.embedding_metadata(&id).unwrap().unwrap();
    assert_eq!(metadata.memory_id, id);
    assert_eq!(metadata.embedder_id, "deterministic-hash-v1:32");
    assert_eq!(metadata.dims, 32);
}

#[test]
fn semantic_query_ranks_relevant_entry_above_irrelevant_entry() {
    let durable = Store::open_memory().unwrap();
    let memory = semantic_store(&durable);

    let irrelevant = memory
        .write(draft(
            EntryKind::Lesson,
            Some("garden"),
            "Tomatoes need steady watering and afternoon shade.",
            1,
        ))
        .unwrap();
    let relevant = memory
        .write(draft(
            EntryKind::Lesson,
            Some("rust-borrow"),
            "Rust borrow checker errors often require changing ownership.",
            2,
        ))
        .unwrap();
    memory.promote(&irrelevant, verdict()).unwrap();
    memory.promote(&relevant, verdict()).unwrap();

    let entries = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            contains: Some("rust borrow ownership error".into()),
            limit: 2,
            ..MemoryQuery::default()
        })
        .unwrap();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, relevant);
    assert_eq!(entries[1].id, irrelevant);
}

#[test]
fn semantic_query_still_enforces_verified_only_default_view() {
    let durable = Store::open_memory().unwrap();
    let memory = semantic_store(&durable);

    let unverified = memory
        .write(draft(
            EntryKind::Lesson,
            Some("rust-close"),
            "Rust borrow checker ownership errors should be fixed directly.",
            1,
        ))
        .unwrap();
    let verified = memory
        .write(draft(
            EntryKind::Lesson,
            Some("garden"),
            "Garden notes are unrelated to compiler diagnostics.",
            2,
        ))
        .unwrap();
    memory.promote(&verified, verdict()).unwrap();

    let entries = memory
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            contains: Some("rust borrow ownership error".into()),
            limit: 4,
            ..MemoryQuery::default()
        })
        .unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, verified);
    assert!(entries.iter().all(|entry| entry.id != unverified));
    assert!(entries.iter().all(|entry| entry.trust == Trust::Verified));
}

#[test]
fn semantic_recall_respects_token_budget() {
    let durable = Store::open_memory().unwrap();
    let memory = semantic_store(&durable);
    let first = memory
        .write(draft(
            EntryKind::Lesson,
            Some("rust-one"),
            "Rust borrow ownership fix needs small lifetimes.",
            1,
        ))
        .unwrap();
    let second = memory
        .write(draft(
            EntryKind::Lesson,
            Some("rust-two"),
            "Rust borrow ownership fix needs explicit references.",
            2,
        ))
        .unwrap();
    memory.promote(&first, verdict()).unwrap();
    memory.promote(&second, verdict()).unwrap();

    let config = MemoryConfig {
        recall: MemoryRecallMode::Semantic,
        lesson_recall_max_lessons: 4,
        lesson_recall_token_budget: 20,
        ..MemoryConfig::default()
    };
    let recall = recall_verified_lessons(&memory, "rust borrow ownership", &config)
        .unwrap()
        .unwrap();

    assert_eq!(recall.entries.len(), 1);
    assert!(recall.approx_tokens <= 20);
}

#[test]
fn reindex_backfills_pre_s1_entries_for_semantic_recall() {
    let durable = Store::open_memory().unwrap();
    let exact = SqliteMemoryStore::from_store(&durable).unwrap();
    let id = exact
        .write(draft(
            EntryKind::Lesson,
            Some("borrow"),
            "Rust borrow checker errors require ownership reasoning.",
            1,
        ))
        .unwrap();
    exact.promote(&id, verdict()).unwrap();
    assert!(exact.embedding_metadata(&id).unwrap().is_none());

    let semantic = semantic_store(&durable);
    let err = semantic
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            contains: Some("rust borrow ownership".into()),
            ..MemoryQuery::default()
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("run `truss memory reindex`"),
        "unexpected error: {err}"
    );

    let summary = semantic.reindex_missing_embeddings().unwrap();
    assert_eq!(summary.scanned, 1);
    assert_eq!(summary.embedded, 1);

    let entries = semantic
        .query(&MemoryQuery {
            kind: Some(EntryKind::Lesson),
            contains: Some("rust borrow ownership".into()),
            ..MemoryQuery::default()
        })
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, id);
}

#[test]
fn exact_default_query_path_keeps_substring_behavior_and_no_vectors() {
    let durable = Store::open_memory().unwrap();
    let memory = SqliteMemoryStore::from_store(&durable).unwrap();
    let excluded = memory
        .write(draft(
            EntryKind::Fact,
            Some("other"),
            "This entry does not contain the marker.",
            1,
        ))
        .unwrap();
    let included = memory
        .write(draft(
            EntryKind::Fact,
            Some("marker"),
            "This entry contains needle-marker exactly.",
            2,
        ))
        .unwrap();
    memory.promote(&excluded, verdict()).unwrap();
    memory.promote(&included, verdict()).unwrap();

    let entries = memory
        .query(&MemoryQuery {
            contains: Some("needle-marker".into()),
            ..MemoryQuery::default()
        })
        .unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, included);
    assert!(memory.embedding_metadata(&included).unwrap().is_none());
    assert!(memory.embedding_metadata(&excluded).unwrap().is_none());
}
