//! Read-only views over the signed durable ledger.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use crate::identity::verify_sig;
use crate::memory::{
    EntryKind, LedgerRange, LedgerRef, MemoryCursor, MemoryEntry, MemoryStore, SqliteMemoryStore,
    Trust,
};
use crate::store::{HumanReviewEntry, HUMAN_REVIEW_KIND};
use crate::store::{JournalEntry, Store, CHILD_ANCHOR_KIND, FANOUT_REDUCTION_KIND};

pub const LEDGER_SCHEMA_VERSION: &str = "truss.ledger.v1";

const REVIEW_LENSES: &[&str] = &["intent", "completeness", "correctness", "shortcuts"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Verified,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub status: VerificationStatus,
    pub verified_entries: usize,
    pub failed_at_seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerSessionSummary {
    pub id: String,
    pub started_at_ms: i64,
    pub head_hash: Option<String>,
    pub entry_count: usize,
    pub run_status: String,
    pub model: Option<String>,
    pub verdict_status: Option<String>,
    pub verification: VerificationReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerEntryView {
    pub seq: i64,
    pub kind: String,
    pub hash: String,
    pub short_hash: String,
    pub signed: bool,
    pub summary: String,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntryPayload {
    pub seq: i64,
    pub kind: String,
    pub hash: String,
    pub short_hash: String,
    pub signed: bool,
    pub payload: serde_json::Value,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerShow {
    pub session_id: String,
    pub head_hash: Option<String>,
    pub verification: VerificationReport,
    pub entries: Vec<LedgerEntryView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerAnchorRef {
    pub log: String,
    pub log_index: u64,
    pub head_hash: String,
    pub entry_hash: String,
    pub anchored_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefinitionOfDoneClause {
    pub kind: String,
    pub desc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect_exit: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_contains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewer_context: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerContractView {
    pub id: Option<String>,
    pub title: Option<String>,
    pub intent: Option<String>,
    pub definition_of_done: Vec<DefinitionOfDoneClause>,
    pub entry: Option<LedgerEntryPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerClauseEvidence {
    pub kind: String,
    pub desc: String,
    pub verdict: String,
    pub evidence: String,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerSelfValidationEvidence {
    pub cmd: Option<String>,
    pub exit: Option<i64>,
    pub summary: Option<String>,
    pub workdir_hash: Option<String>,
    pub evidence_only: bool,
    pub verdict_unchanged: bool,
    pub entry: LedgerEntryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelMajority {
    pub met_count: usize,
    pub reviewer_count: usize,
    pub decision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelReviewerVerdict {
    pub seat: usize,
    pub lens: String,
    pub reviewer: Option<String>,
    pub verdict: String,
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerPanelResult {
    pub desc: String,
    pub clause_verdict: String,
    pub majority: Option<PanelMajority>,
    pub reviewers: Vec<PanelReviewerVerdict>,
    pub evidence: String,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEvidenceView {
    pub checks: Vec<LedgerClauseEvidence>,
    pub self_validation: Vec<LedgerSelfValidationEvidence>,
    pub panel: Vec<LedgerPanelResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerHumanSignoff {
    pub decision: crate::store::HumanReviewDecision,
    pub note: Option<String>,
    pub attested_head_hash: String,
    pub reviewer_key_id: String,
    pub attested_head_matches_current: bool,
    pub current_reviewable_head_hash: Option<String>,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerVerdictBundle {
    pub session_id: String,
    pub contract: Option<LedgerContractView>,
    pub evidence: LedgerEvidenceView,
    pub recorded_overall: Option<String>,
    pub overall: String,
    pub verdict_entry: Option<LedgerEntryPayload>,
    pub human_signoff: Option<LedgerHumanSignoff>,
    pub head_hash: Option<String>,
    pub anchor_refs: Vec<LedgerAnchorRef>,
    pub verification: VerificationReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedLedgerEntry {
    pub seq: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub prev_hash: String,
    pub entry_hash: String,
    pub signature: String,
    pub signed: bool,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedLedgerSession {
    pub session_id: String,
    pub pubkey_id: Option<String>,
    pub head_hash: Option<String>,
    pub verification: VerificationReport,
    pub entries: Vec<SignedLedgerEntry>,
    pub anchor_refs: Vec<LedgerAnchorRef>,
    pub child_sessions: Vec<ExportedLedgerSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineVerification {
    pub algorithm: String,
    pub signature_algorithm: String,
    pub status: VerificationStatus,
    pub verified_entries: usize,
    pub failed_session_id: Option<String>,
    pub failed_at_seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerExportBundle {
    pub schema_version: String,
    pub command: String,
    pub contract: Option<LedgerContractView>,
    pub evidence: LedgerEvidenceView,
    pub verdict: LedgerVerdictBundle,
    pub ledger: ExportedLedgerSession,
    pub offline_verification: OfflineVerification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerVerifyReport {
    pub session_id: String,
    pub head_hash: Option<String>,
    pub verification: VerificationReport,
    pub entries: Vec<LedgerEntryPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerMemoryEntryView {
    pub id: String,
    pub kind: EntryKind,
    pub key: Option<String>,
    pub body: String,
    pub trust: Trust,
    pub produced_by: LedgerRef,
    pub verified_by_ref: Option<LedgerRef>,
    pub sources: Vec<LedgerRef>,
    pub created_ms: i64,
    pub verification_status: VerificationStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerCompactionView {
    pub summary_memory_id: Option<String>,
    pub mode: Option<String>,
    pub elided_range: Option<LedgerRange>,
    pub original_entry_count: usize,
    pub recoverable: bool,
    pub entry: LedgerEntryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerSummaryExpansion {
    pub summary_memory_id: String,
    pub elided_range: LedgerRange,
    pub entries: Vec<LedgerEntryPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecall {
    pub session_id: String,
    pub head_hash: Option<String>,
    pub verification: VerificationReport,
    pub memory_entries: Vec<LedgerMemoryEntryView>,
    pub compactions: Vec<LedgerCompactionView>,
    pub expanded_summary: Option<LedgerSummaryExpansion>,
}

#[derive(Clone)]
pub struct LedgerReader {
    store: Store,
}

impl LedgerReader {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn get_session(&self, id: &str) -> Result<Option<crate::store::SessionRow>> {
        self.store.get_session(id)
    }

    pub fn entries(&self, id: &str) -> Result<Vec<JournalEntry>> {
        self.store.entries(id)
    }

    pub fn last_entry(&self, id: &str) -> Result<Option<JournalEntry>> {
        self.store.last_entry(id)
    }

    pub fn head_hash_hex(&self, id: &str) -> Result<Option<String>> {
        self.store.head_hash_hex(id)
    }

    pub fn anchor_count(&self, id: &str) -> Result<i64> {
        self.store.anchor_count(id)
    }

    pub fn verify_chain(&self, id: &str) -> Result<VerificationReport> {
        self.verification_report(id, None)
    }

    pub fn verify(&self, id: &str, expect_pubkey: Option<&str>) -> Result<LedgerVerifyReport> {
        ensure_session_exists(&self.store, id)?;
        let verification = self.verification_report(id, expect_pubkey)?;
        let entries = self
            .store
            .entries(id)?
            .iter()
            .map(|entry| entry_payload(entry, &verification))
            .collect();
        Ok(LedgerVerifyReport {
            session_id: id.to_string(),
            head_hash: self.store.head_hash_hex(id)?,
            verification,
            entries,
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<LedgerSessionSummary>> {
        self.store
            .list_sessions()?
            .into_iter()
            .map(|row| {
                let entries = self.store.entries(&row.id)?;
                let verification = self.verification_report(&row.id, None)?;
                Ok(LedgerSessionSummary {
                    id: row.id,
                    started_at_ms: row.created_at,
                    head_hash: row.head_hash.map(hex::encode),
                    entry_count: row.entry_count,
                    run_status: row.status,
                    model: row.model,
                    verdict_status: derive_verdict_status(&entries),
                    verification,
                })
            })
            .collect()
    }

    pub fn show(&self, id: &str) -> Result<LedgerShow> {
        ensure_session_exists(&self.store, id)?;
        let entries = self.store.entries(id)?;
        let verification = self.verification_report(id, None)?;
        let head_hash = self.store.head_hash_hex(id)?;
        let entry_views = entries
            .iter()
            .map(|entry| entry_view(entry, &verification))
            .collect();
        Ok(LedgerShow {
            session_id: id.to_string(),
            head_hash,
            verification,
            entries: entry_views,
        })
    }

    pub fn get(&self, id: &str, seq: i64) -> Result<LedgerEntryPayload> {
        ensure_session_exists(&self.store, id)?;
        let entries = self.store.entries(id)?;
        let verification = self.verification_report(id, None)?;
        let entry = entries
            .iter()
            .find(|entry| entry.seq == seq)
            .ok_or_else(|| anyhow!("entry {seq} not found in session {id}"))?;
        Ok(entry_payload(entry, &verification))
    }

    pub fn verdict(&self, id: &str) -> Result<LedgerVerdictBundle> {
        ensure_session_exists(&self.store, id)?;
        let entries = self.store.entries(id)?;
        let verification = self.verification_report(id, None)?;
        verdict_bundle(
            id,
            &entries,
            &verification,
            self.store.head_hash_hex(id)?,
            self.anchor_refs(id)?,
        )
    }

    pub fn export(&self, id: &str) -> Result<LedgerExportBundle> {
        let verdict = self.verdict(id)?;
        let mut visited = HashSet::new();
        let ledger = self.export_session(id, &mut visited)?;
        let offline_verification = verify_export_bundle_session(&ledger);
        Ok(LedgerExportBundle {
            schema_version: LEDGER_SCHEMA_VERSION.to_string(),
            command: "export".to_string(),
            contract: verdict.contract.clone(),
            evidence: verdict.evidence.clone(),
            verdict,
            ledger,
            offline_verification,
        })
    }

    pub fn recall(&self, id: &str, summary_id: Option<&str>) -> Result<LedgerRecall> {
        ensure_session_exists(&self.store, id)?;
        let entries = self.store.entries(id)?;
        let verification = self.verification_report(id, None)?;
        let memory_entries = self.memory_entries_for_session(id, &verification)?;
        let compactions = entries
            .iter()
            .filter(|entry| entry.kind == "compaction_performed")
            .map(|entry| compaction_view(entry, &entries, &verification))
            .collect::<Vec<_>>();
        let expanded_summary = summary_id
            .map(|summary_id| expand_summary(summary_id, &compactions, &entries, &verification))
            .transpose()?;

        Ok(LedgerRecall {
            session_id: id.to_string(),
            head_hash: self.store.head_hash_hex(id)?,
            verification,
            memory_entries,
            compactions,
            expanded_summary,
        })
    }

    fn export_session(
        &self,
        id: &str,
        visited: &mut HashSet<String>,
    ) -> Result<ExportedLedgerSession> {
        ensure_session_exists(&self.store, id)?;
        if !visited.insert(id.to_string()) {
            return Ok(ExportedLedgerSession {
                session_id: id.to_string(),
                pubkey_id: self.store.get_session(id)?.and_then(|s| s.pubkey_id),
                head_hash: self.store.head_hash_hex(id)?,
                verification: verification_report(Ok(0)),
                entries: Vec::new(),
                anchor_refs: self.anchor_refs(id)?,
                child_sessions: Vec::new(),
            });
        }
        let session = self.store.get_session(id)?.expect("session checked above");
        let entries = self.store.entries(id)?;
        let verification = self.verification_report(id, None)?;
        let child_ids = child_session_ids(&entries);
        let mut child_sessions = Vec::new();
        for child in child_ids {
            if self.store.get_session(&child)?.is_some() {
                child_sessions.push(self.export_session(&child, visited)?);
            }
        }
        Ok(ExportedLedgerSession {
            session_id: id.to_string(),
            pubkey_id: session.pubkey_id,
            head_hash: self.store.head_hash_hex(id)?,
            verification: verification.clone(),
            entries: entries
                .iter()
                .map(|entry| signed_entry(entry, &verification))
                .collect(),
            anchor_refs: self.anchor_refs(id)?,
            child_sessions,
        })
    }

    fn anchor_refs(&self, id: &str) -> Result<Vec<LedgerAnchorRef>> {
        self.store
            .anchors(id)?
            .into_iter()
            .map(|anchor| {
                Ok(LedgerAnchorRef {
                    log: anchor.log,
                    log_index: anchor.log_index,
                    head_hash: anchor.head_hash,
                    entry_hash: anchor.entry_hash,
                    anchored_at_ms: anchor.anchored_at,
                })
            })
            .collect()
    }

    fn verification_report(
        &self,
        id: &str,
        expect_pubkey: Option<&str>,
    ) -> Result<VerificationReport> {
        let report = verification_report(self.store.verify_chain_pinned(id, expect_pubkey)?);
        if report.status == VerificationStatus::Unverified {
            return Ok(report);
        }
        let session = self.store.get_session(id)?;
        let entries = self.store.entries(id)?;
        if !entries.is_empty()
            && session
                .as_ref()
                .and_then(|s| s.pubkey_id.as_ref())
                .is_none()
        {
            return Ok(verification_report(Err(0)));
        }
        if let Some(unsigned) = entries.iter().find(|entry| entry.signature.is_empty()) {
            return Ok(verification_report(Err(unsigned.seq)));
        }
        Ok(report)
    }

    fn memory_entries_for_session(
        &self,
        id: &str,
        verification: &VerificationReport,
    ) -> Result<Vec<LedgerMemoryEntryView>> {
        if !memory_table_exists(&self.store)? {
            return Ok(Vec::new());
        }
        let memory = SqliteMemoryStore::from_existing_store(&self.store);
        let mut entries = memory
            .delta_since(MemoryCursor(0))
            .map_err(|e| anyhow!(e.to_string()))?
            .entries
            .into_iter()
            .filter(|entry| memory_entry_references_session(entry, id))
            .map(|entry| memory_entry_view(entry, verification))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.produced_by
                .seq
                .cmp(&right.produced_by.seq)
                .then(left.id.cmp(&right.id))
        });
        Ok(entries)
    }
}

fn memory_table_exists(store: &Store) -> Result<bool> {
    let conn = store.connection();
    let conn = conn.lock().unwrap();
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'memory'",
        [],
        |row| row.get(0),
    )?;
    Ok(exists > 0)
}

fn ensure_session_exists(store: &Store, id: &str) -> Result<()> {
    if store.get_session(id)?.is_some() {
        Ok(())
    } else {
        Err(anyhow!("session {id} not found"))
    }
}

fn verification_report(result: std::result::Result<usize, i64>) -> VerificationReport {
    match result {
        Ok(n) => VerificationReport {
            status: VerificationStatus::Verified,
            verified_entries: n,
            failed_at_seq: None,
        },
        Err(seq) => VerificationReport {
            status: VerificationStatus::Unverified,
            verified_entries: seq.max(0) as usize,
            failed_at_seq: Some(seq),
        },
    }
}

fn verification_for_entry(seq: i64, report: &VerificationReport) -> VerificationStatus {
    match report.failed_at_seq {
        None => report.status,
        Some(failed) if seq < failed => VerificationStatus::Verified,
        Some(_) => VerificationStatus::Unverified,
    }
}

fn entry_view(entry: &JournalEntry, report: &VerificationReport) -> LedgerEntryView {
    let hash = hex::encode(&entry.entry_hash);
    LedgerEntryView {
        seq: entry.seq,
        kind: entry.kind.clone(),
        short_hash: short_hash(&hash),
        hash,
        signed: !entry.signature.is_empty(),
        summary: summarize_payload(&entry.kind, &entry.payload),
        verification_status: verification_for_entry(entry.seq, report),
    }
}

fn entry_payload(entry: &JournalEntry, report: &VerificationReport) -> LedgerEntryPayload {
    let hash = hex::encode(&entry.entry_hash);
    LedgerEntryPayload {
        seq: entry.seq,
        kind: entry.kind.clone(),
        short_hash: short_hash(&hash),
        hash,
        signed: !entry.signature.is_empty(),
        payload: entry.payload.clone(),
        verification_status: verification_for_entry(entry.seq, report),
    }
}

fn signed_entry(entry: &JournalEntry, report: &VerificationReport) -> SignedLedgerEntry {
    SignedLedgerEntry {
        seq: entry.seq,
        kind: entry.kind.clone(),
        payload: entry.payload.clone(),
        prev_hash: hex::encode(&entry.prev_hash),
        entry_hash: hex::encode(&entry.entry_hash),
        signature: hex::encode(&entry.signature),
        signed: !entry.signature.is_empty(),
        verification_status: verification_for_entry(entry.seq, report),
    }
}

fn memory_entry_references_session(entry: &MemoryEntry, id: &str) -> bool {
    entry.provenance.produced_by.session == id
        || entry
            .provenance
            .sources
            .iter()
            .any(|source| source.session == id)
        || entry
            .provenance
            .verified_by_ref
            .as_ref()
            .is_some_and(|ledger| ledger.session == id)
}

fn memory_entry_view(
    entry: MemoryEntry,
    verification: &VerificationReport,
) -> LedgerMemoryEntryView {
    let produced_status = verification_for_ref(&entry.provenance.produced_by, verification);
    let verdict_status = entry
        .provenance
        .verified_by_ref
        .as_ref()
        .map(|ledger| verification_for_ref(ledger, verification))
        .unwrap_or(VerificationStatus::Verified);
    let verification_status = if produced_status == VerificationStatus::Verified
        && verdict_status == VerificationStatus::Verified
    {
        VerificationStatus::Verified
    } else {
        VerificationStatus::Unverified
    };
    LedgerMemoryEntryView {
        id: entry.id.0,
        kind: entry.kind,
        key: entry.key,
        body: entry.body,
        trust: entry.trust,
        produced_by: entry.provenance.produced_by,
        verified_by_ref: entry.provenance.verified_by_ref,
        sources: entry.provenance.sources,
        created_ms: entry.created_ms,
        verification_status,
    }
}

fn verification_for_ref(
    ledger: &LedgerRef,
    verification: &VerificationReport,
) -> VerificationStatus {
    verification_for_entry(ledger.seq, verification)
}

fn compaction_view(
    entry: &JournalEntry,
    entries: &[JournalEntry],
    verification: &VerificationReport,
) -> LedgerCompactionView {
    let elided_range = serde_json::from_value::<LedgerRange>(
        entry
            .payload
            .get("elided_range")
            .cloned()
            .unwrap_or_default(),
    )
    .ok();
    let original_entry_count = elided_range
        .as_ref()
        .map(|range| entries_in_range(entries, range).len())
        .unwrap_or_default();
    LedgerCompactionView {
        summary_memory_id: string_field(&entry.payload, "summary_memory_id"),
        mode: string_field(&entry.payload, "mode"),
        recoverable: elided_range.is_some() && original_entry_count > 0,
        elided_range,
        original_entry_count,
        entry: entry_payload(entry, verification),
    }
}

fn expand_summary(
    summary_id: &str,
    compactions: &[LedgerCompactionView],
    entries: &[JournalEntry],
    verification: &VerificationReport,
) -> Result<LedgerSummaryExpansion> {
    let compaction = compactions
        .iter()
        .find(|compaction| compaction.summary_memory_id.as_deref() == Some(summary_id))
        .ok_or_else(|| anyhow!("summary {summary_id} not found in compaction entries"))?;
    let range = compaction
        .elided_range
        .clone()
        .ok_or_else(|| anyhow!("summary {summary_id} has no elided range"))?;
    let entries = entries_in_range(entries, &range)
        .into_iter()
        .map(|entry| entry_payload(entry, verification))
        .collect();
    Ok(LedgerSummaryExpansion {
        summary_memory_id: summary_id.to_string(),
        elided_range: range,
        entries,
    })
}

fn entries_in_range<'a>(entries: &'a [JournalEntry], range: &LedgerRange) -> Vec<&'a JournalEntry> {
    entries
        .iter()
        .filter(|entry| entry.seq >= range.start.seq && entry.seq <= range.end.seq)
        .collect()
}

fn verdict_bundle(
    id: &str,
    entries: &[JournalEntry],
    verification: &VerificationReport,
    head_hash: Option<String>,
    anchor_refs: Vec<LedgerAnchorRef>,
) -> Result<LedgerVerdictBundle> {
    let contract_entry = entries.iter().rev().find(|entry| entry.kind == "contract");
    let verdict_entry = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == "verdict" || entry.kind == "memory_verdict");
    let current_reviewable_head_hash = entries
        .iter()
        .rev()
        .find(|entry| entry.kind != HUMAN_REVIEW_KIND)
        .map(|entry| hex::encode(&entry.entry_hash));
    let human_signoff = entries
        .iter()
        .rev()
        .find(|entry| entry.kind == HUMAN_REVIEW_KIND)
        .and_then(|entry| human_signoff(entry, verification, current_reviewable_head_hash.clone()));
    let verdict_payload = verdict_entry.map(|entry| {
        if entry.kind == "memory_verdict" {
            entry
                .payload
                .get("verdict")
                .unwrap_or(&entry.payload)
                .clone()
        } else {
            entry.payload.clone()
        }
    });
    let clause_values = verdict_payload
        .as_ref()
        .and_then(|payload| payload.get("clauses").or_else(|| payload.get("results")))
        .and_then(|clauses| clauses.as_array())
        .cloned()
        .unwrap_or_default();
    let definition_of_done = definition_of_done(contract_entry, &clause_values);
    let contract = contract_entry.map(|entry| LedgerContractView {
        id: string_field(&entry.payload, "id"),
        title: string_field(&entry.payload, "title"),
        intent: string_field(&entry.payload, "intent"),
        definition_of_done: definition_of_done.clone(),
        entry: Some(entry_payload(entry, verification)),
    });

    let mut checks = Vec::new();
    let mut panels = Vec::new();
    for clause in &clause_values {
        let kind = string_field(clause, "kind").unwrap_or_else(|| "unknown".into());
        let desc = string_field(clause, "desc").unwrap_or_default();
        let verdict = normalize_verdict(string_field(clause, "verdict").as_deref());
        let evidence = string_field(clause, "evidence").unwrap_or_default();
        let verification_status = verdict_entry
            .map(|entry| verification_for_entry(entry.seq, verification))
            .unwrap_or(VerificationStatus::Unverified);
        if kind == "check" {
            checks.push(LedgerClauseEvidence {
                kind,
                desc,
                verdict,
                evidence,
                verification_status,
            });
        } else if kind == "review" {
            panels.push(panel_result(desc, verdict, evidence, verification_status));
        }
    }

    let self_validation = entries
        .iter()
        .filter(|entry| entry.kind == "self_validation")
        .map(|entry| LedgerSelfValidationEvidence {
            cmd: string_field(&entry.payload, "cmd"),
            exit: entry.payload.get("exit").and_then(|v| v.as_i64()),
            summary: string_field(&entry.payload, "summary"),
            workdir_hash: string_field(&entry.payload, "workdir_hash"),
            evidence_only: entry
                .payload
                .get("evidence_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            verdict_unchanged: entry
                .payload
                .get("verdict_unchanged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            entry: entry_payload(entry, verification),
        })
        .collect();

    let recorded_overall = verdict_payload
        .as_ref()
        .and_then(|payload| string_field(payload, "overall"))
        .map(|s| normalize_verdict(Some(&s)));
    let overall = if verification.status == VerificationStatus::Verified {
        recorded_overall
            .clone()
            .unwrap_or_else(|| "UNVERIFIABLE".into())
    } else {
        "UNVERIFIABLE".into()
    };

    Ok(LedgerVerdictBundle {
        session_id: id.to_string(),
        contract,
        evidence: LedgerEvidenceView {
            checks,
            self_validation,
            panel: panels,
        },
        recorded_overall,
        overall,
        verdict_entry: verdict_entry.map(|entry| entry_payload(entry, verification)),
        human_signoff,
        head_hash,
        anchor_refs,
        verification: verification.clone(),
    })
}

fn human_signoff(
    entry: &JournalEntry,
    verification: &VerificationReport,
    current_reviewable_head_hash: Option<String>,
) -> Option<LedgerHumanSignoff> {
    let review = serde_json::from_value::<HumanReviewEntry>(entry.payload.clone()).ok()?;
    let attested_head_matches_current =
        current_reviewable_head_hash.as_deref() == Some(review.attested_head_hash.as_str());
    Some(LedgerHumanSignoff {
        decision: review.decision,
        note: review.note,
        attested_head_hash: review.attested_head_hash,
        reviewer_key_id: review.reviewer_key_id,
        attested_head_matches_current,
        current_reviewable_head_hash,
        verification_status: verification_for_entry(entry.seq, verification),
    })
}

fn definition_of_done(
    contract_entry: Option<&JournalEntry>,
    clause_values: &[serde_json::Value],
) -> Vec<DefinitionOfDoneClause> {
    if let Some(done) = contract_entry
        .and_then(|entry| entry.payload.get("done"))
        .and_then(|done| done.as_array())
    {
        let clauses = done
            .iter()
            .map(definition_clause)
            .collect::<Vec<DefinitionOfDoneClause>>();
        if !clauses.is_empty() {
            return clauses;
        }
    }
    clause_values.iter().map(definition_clause).collect()
}

fn definition_clause(value: &serde_json::Value) -> DefinitionOfDoneClause {
    DefinitionOfDoneClause {
        kind: string_field(value, "kind").unwrap_or_else(|| "unknown".into()),
        desc: string_field(value, "desc").unwrap_or_default(),
        run: string_field(value, "run"),
        expect_exit: value.get("expect_exit").and_then(|v| v.as_i64()),
        expect_contains: string_array_field(value, "expect_contains"),
        reviewer_context: string_array_field(value, "reviewer_context"),
    }
}

fn panel_result(
    desc: String,
    clause_verdict: String,
    evidence: String,
    verification_status: VerificationStatus,
) -> LedgerPanelResult {
    let mut lines = evidence.lines();
    let majority = lines.next().and_then(parse_panel_majority);
    let reviewers = lines
        .filter_map(parse_panel_vote)
        .collect::<Vec<PanelReviewerVerdict>>();
    LedgerPanelResult {
        desc,
        clause_verdict,
        majority,
        reviewers,
        evidence,
        verification_status,
    }
}

fn parse_panel_majority(line: &str) -> Option<PanelMajority> {
    let rest = line.strip_prefix("panel: ")?;
    let counts = rest.split_whitespace().next()?;
    let (met, total) = counts.split_once('/')?;
    let met_count = met.parse::<usize>().ok()?;
    let reviewer_count = total.parse::<usize>().ok()?;
    let decision = if met_count * 2 > reviewer_count {
        "PASS"
    } else if reviewer_count == 0 {
        "UNVERIFIABLE"
    } else {
        "FAIL"
    };
    Some(PanelMajority {
        met_count,
        reviewer_count,
        decision: decision.into(),
    })
}

fn parse_panel_vote(line: &str) -> Option<PanelReviewerVerdict> {
    let trimmed = line.trim();
    let end = trimmed.find(']')?;
    let header = trimmed.get(1..end)?;
    let body = trimmed.get(end + 1..)?.trim();
    let (seat_text, reviewer) = header.split_once('/')?;
    let seat = seat_text.parse::<usize>().ok()?;
    let (verdict_raw, reasoning) = body
        .split_once(" — ")
        .map(|(v, r)| (v.trim(), r.trim()))
        .unwrap_or((body, ""));
    let verdict = match verdict_raw {
        "met" => "MET",
        "REFUTED" => "REFUTED",
        raw if raw.starts_with("unparseable") => "UNVERIFIABLE",
        raw if raw.starts_with("error") => "UNVERIFIABLE",
        _ => "UNVERIFIABLE",
    };
    Some(PanelReviewerVerdict {
        seat,
        lens: REVIEW_LENSES[(seat - 1) % REVIEW_LENSES.len()].to_string(),
        reviewer: Some(reviewer.to_string()),
        verdict: verdict.into(),
        reasoning: if reasoning.is_empty() {
            verdict_raw.to_string()
        } else {
            reasoning.to_string()
        },
    })
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn string_array_field(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_verdict(raw: Option<&str>) -> String {
    match raw.unwrap_or("").to_ascii_lowercase().as_str() {
        "pass" | "passed" => "PASS".into(),
        "fail" | "failed" => "FAIL".into(),
        "unverifiable" => "UNVERIFIABLE".into(),
        "" => "UNVERIFIABLE".into(),
        other => other.to_ascii_uppercase(),
    }
}

pub fn verify_export_bundle(bundle: &LedgerExportBundle) -> OfflineVerification {
    verify_export_bundle_session(&bundle.ledger)
}

fn verify_export_bundle_session(session: &ExportedLedgerSession) -> OfflineVerification {
    let child_map = flatten_child_sessions(session);
    let mut visited = HashSet::new();
    match verify_export_session_recursive(session, &child_map, &mut visited) {
        Ok(verified_entries) => OfflineVerification {
            algorithm: "sha256(prev_hash || kind || canonical_json(payload) || seq_le)".into(),
            signature_algorithm: "ed25519".into(),
            status: VerificationStatus::Verified,
            verified_entries,
            failed_session_id: None,
            failed_at_seq: None,
        },
        Err((session_id, seq)) => OfflineVerification {
            algorithm: "sha256(prev_hash || kind || canonical_json(payload) || seq_le)".into(),
            signature_algorithm: "ed25519".into(),
            status: VerificationStatus::Unverified,
            verified_entries: 0,
            failed_session_id: Some(session_id),
            failed_at_seq: Some(seq),
        },
    }
}

fn flatten_child_sessions(
    session: &ExportedLedgerSession,
) -> HashMap<&str, &ExportedLedgerSession> {
    fn walk<'a>(
        session: &'a ExportedLedgerSession,
        out: &mut HashMap<&'a str, &'a ExportedLedgerSession>,
    ) {
        for child in &session.child_sessions {
            out.insert(child.session_id.as_str(), child);
            walk(child, out);
        }
    }
    let mut out = HashMap::new();
    walk(session, &mut out);
    out
}

fn verify_export_session_recursive(
    session: &ExportedLedgerSession,
    child_map: &HashMap<&str, &ExportedLedgerSession>,
    visited: &mut HashSet<String>,
) -> std::result::Result<usize, (String, i64)> {
    if !visited.insert(session.session_id.clone()) {
        return Ok(0);
    }

    let pubkey = session.pubkey_id.as_deref();
    let mut prev = vec![0u8; 32];
    let mut verified = 0usize;
    for entry in &session.entries {
        let prev_hash =
            hex::decode(&entry.prev_hash).map_err(|_| (session.session_id.clone(), entry.seq))?;
        if prev_hash != prev {
            return Err((session.session_id.clone(), entry.seq));
        }
        let expected_hash = hash_export_entry(&prev, &entry.kind, &entry.payload, entry.seq);
        let entry_hash =
            hex::decode(&entry.entry_hash).map_err(|_| (session.session_id.clone(), entry.seq))?;
        if expected_hash != entry_hash {
            return Err((session.session_id.clone(), entry.seq));
        }
        let signature =
            hex::decode(&entry.signature).map_err(|_| (session.session_id.clone(), entry.seq))?;
        let signature_pubkey = if entry.kind == HUMAN_REVIEW_KIND {
            entry
                .payload
                .get("reviewer_key_id")
                .and_then(|value| value.as_str())
        } else {
            pubkey
        };
        let Some(signature_pubkey) = signature_pubkey else {
            return Err((session.session_id.clone(), entry.seq));
        };
        if signature.is_empty() || !verify_sig(signature_pubkey, &entry_hash, &signature) {
            return Err((session.session_id.clone(), entry.seq));
        }
        if entry.kind == CHILD_ANCHOR_KIND
            && !verify_export_child_anchor(&entry.payload, child_map, visited)?
        {
            return Err((session.session_id.clone(), entry.seq));
        }
        if entry.kind == FANOUT_REDUCTION_KIND
            && !verify_export_fanout_reduction(session, &entry.payload)
        {
            return Err((session.session_id.clone(), entry.seq));
        }
        prev = entry_hash;
        verified += 1;
    }
    if let Some(head_hash) = &session.head_hash {
        match session.entries.last() {
            Some(last) if &last.entry_hash != head_hash => {
                return Err((session.session_id.clone(), last.seq));
            }
            None => return Err((session.session_id.clone(), 0)),
            _ => {}
        }
    }
    Ok(verified)
}

fn verify_export_child_anchor(
    payload: &serde_json::Value,
    child_map: &HashMap<&str, &ExportedLedgerSession>,
    visited: &mut HashSet<String>,
) -> std::result::Result<bool, (String, i64)> {
    let Some(child_session) = payload.get("child_run_id").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    let Some(expected_head) = payload.get("child_head_hash").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    let Some(child) = child_map.get(child_session) else {
        return Ok(false);
    };
    verify_export_session_recursive(child, child_map, visited)?;
    if child.head_hash.as_deref() != Some(expected_head) {
        return Ok(false);
    }
    if let Some(expected_root) = payload.get("child_root_hash").and_then(|v| v.as_str()) {
        let Some(root) = child.entries.first().map(|entry| entry.entry_hash.as_str()) else {
            return Ok(false);
        };
        if root != expected_root {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verify_export_fanout_reduction(
    session: &ExportedLedgerSession,
    payload: &serde_json::Value,
) -> bool {
    if payload.get("parent_run_id").and_then(|v| v.as_str()) != Some(session.session_id.as_str()) {
        return false;
    }
    let Some(children) = payload.get("children").and_then(|v| v.as_array()) else {
        return false;
    };
    if children.is_empty() {
        return false;
    }
    let mut child_ids = HashSet::new();
    for child in children {
        let Some(child_run_id) = child.get("child_run_id").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(status) = child.get("status").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(anchor_seq) = child
            .get("parent_anchor")
            .and_then(|v| v.get("seq"))
            .and_then(|v| v.as_i64())
        else {
            return false;
        };
        let Some(anchor) = session
            .entries
            .iter()
            .find(|entry| entry.seq == anchor_seq && entry.kind == CHILD_ANCHOR_KIND)
        else {
            return false;
        };
        if anchor.payload.get("child_run_id").and_then(|v| v.as_str()) != Some(child_run_id) {
            return false;
        }
        if anchor.payload.get("status").and_then(|v| v.as_str()) != Some(status) {
            return false;
        }
        if child.get("child_head_hash").and_then(|v| v.as_str())
            != anchor
                .payload
                .get("child_head_hash")
                .and_then(|v| v.as_str())
        {
            return false;
        }
        if !ledger_ref_values_match(child.get("verdict_ref"), anchor.payload.get("verdict_ref")) {
            return false;
        }
        child_ids.insert(child_run_id.to_string());
    }
    if let Some(winner) = payload.get("winner") {
        if winner.is_null() {
            return true;
        }
        let Some(winner_child) = winner.get("child_run_id").and_then(|v| v.as_str()) else {
            return false;
        };
        if !child_ids.contains(winner_child) {
            return false;
        }
        let Some(matching_child) = children
            .iter()
            .find(|child| child.get("child_run_id").and_then(|v| v.as_str()) == Some(winner_child))
        else {
            return false;
        };
        if matching_child.get("status").and_then(|v| v.as_str()) != Some("completed") {
            return false;
        }
        if !ledger_ref_values_match(winner.get("verdict_ref"), matching_child.get("verdict_ref")) {
            return false;
        }
    }
    true
}

fn child_session_ids(entries: &[JournalEntry]) -> Vec<String> {
    let mut ids = Vec::new();
    for entry in entries {
        if entry.kind == CHILD_ANCHOR_KIND {
            if let Some(child) = entry.payload.get("child_run_id").and_then(|v| v.as_str()) {
                if !ids.iter().any(|existing| existing == child) {
                    ids.push(child.to_string());
                }
            }
        }
    }
    ids
}

fn hash_export_entry(prev: &[u8], kind: &str, payload: &serde_json::Value, seq: i64) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(kind.as_bytes());
    h.update(serde_json::to_vec(payload).unwrap_or_default());
    h.update(seq.to_le_bytes());
    h.finalize().to_vec()
}

fn ledger_ref_values_match(
    left: Option<&serde_json::Value>,
    right: Option<&serde_json::Value>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(l), Some(r)) if l.is_null() && r.is_null() => true,
        (Some(l), Some(r)) => {
            l.get("session").and_then(|v| v.as_str()) == r.get("session").and_then(|v| v.as_str())
                && l.get("seq").and_then(|v| v.as_i64()) == r.get("seq").and_then(|v| v.as_i64())
        }
        _ => false,
    }
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

fn derive_verdict_status(entries: &[JournalEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.kind == "verdict" || entry.kind == "memory_verdict")
        .and_then(|entry| entry.payload.get("overall"))
        .and_then(|overall| overall.as_str())
        .map(|overall| overall.to_ascii_lowercase())
}

fn summarize_payload(kind: &str, payload: &serde_json::Value) -> String {
    for key in [
        "summary",
        "prompt",
        "final_text",
        "content",
        "name",
        "overall",
    ] {
        if let Some(text) = payload.get(key).and_then(|v| v.as_str()) {
            return truncate_one_line(text, 96);
        }
    }
    if kind == "tool_result" {
        if let Some(name) = payload.get("name").and_then(|v| v.as_str()) {
            let is_error = payload
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            return format!("{name} result ({})", if is_error { "error" } else { "ok" });
        }
    }
    if kind == "model_result" {
        if let Some(stop) = payload.get("stop_reason").and_then(|v| v.as_str()) {
            return format!("model stop: {stop}");
        }
    }
    truncate_one_line(&payload.to_string(), 96)
}

fn truncate_one_line(text: &str, max_chars: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let mut truncated = one_line
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push_str("...");
    truncated
}
