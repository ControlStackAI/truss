//! Authenticated ai-memory broker backend for Truss governed memory.
//!
//! The broker socket is selected by the operating system. Requests never carry
//! an identity claim. Canonical records remain the source of truth; Truss keeps
//! its signed execution journal separately as provenance.

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::store::{MemoryError, MemoryStore};
use super::types::{
    EntryKind, LedgerRef, MemoryCursor, MemoryDelta, MemoryDraft, MemoryEntry, MemoryId,
    MemoryQuery, PolicyDraft, PolicyId, PolicyRecord, Provenance, Trust,
};
use crate::delegation::Verdict;

const PROTOCOL_VERSION: u8 = 1;
const MEMORY_SCOPE: &str = "mem:memories";
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct AiMemoryStore {
    socket_path: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrokerRequest<'a> {
    protocol_version: u8,
    id: String,
    operation: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_content_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mutations: Option<Vec<BrokerMutation>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrokerMutation {
    key: String,
    value: Value,
    expected_content_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrokerResponse {
    protocol_version: u8,
    id: String,
    ok: bool,
    result: Option<Value>,
    error: Option<BrokerFailure>,
}

#[derive(Debug, Deserialize)]
struct BrokerFailure {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionedValue {
    content_hash: String,
    value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CanonicalTrussEntry {
    adapter: String,
    schema_version: u32,
    id: MemoryId,
    kind: EntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    content: String,
    trust: Trust,
    provenance: Provenance,
    salience: f32,
    created_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<MemoryId>,
    #[serde(rename = "sourceRecordIds")]
    source_record_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CanonicalTrussPolicy {
    adapter: String,
    schema_version: u32,
    record: PolicyRecord,
    #[serde(rename = "sourceRecordIds")]
    source_record_ids: Vec<String>,
}

impl AiMemoryStore {
    pub fn new(socket_path: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        Self::with_timeout(socket_path, Duration::from_secs(5))
    }

    pub fn with_timeout(
        socket_path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, MemoryError> {
        let socket_path = socket_path.into();
        if !socket_path.is_absolute() {
            return Err(backend("ai-memory broker socket path must be absolute"));
        }
        if timeout.is_zero() {
            return Err(backend("ai-memory broker timeout must be positive"));
        }
        Ok(Self {
            socket_path,
            timeout,
        })
    }

    fn request<T: DeserializeOwned>(
        &self,
        operation: &str,
        scope: Option<&str>,
        key: Option<&str>,
        value: Option<Value>,
        expected_content_hash: Option<&str>,
    ) -> Result<T, MemoryError> {
        let id = format!(
            "truss-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let request = BrokerRequest {
            protocol_version: PROTOCOL_VERSION,
            id: id.clone(),
            operation,
            scope,
            key,
            value,
            expected_content_hash,
            mutations: None,
        };
        let response = exchange(&self.socket_path, self.timeout, &request)?;
        if response.protocol_version != PROTOCOL_VERSION || response.id != id {
            return Err(backend("invalid ai-memory broker response correlation"));
        }
        if !response.ok {
            let failure = response
                .error
                .ok_or_else(|| backend("ai-memory broker returned an invalid error"))?;
            return Err(backend(format!(
                "ai-memory broker {}: {}",
                failure.code, failure.message
            )));
        }
        serde_json::from_value(response.result.unwrap_or(Value::Null)).map_err(MemoryError::Json)
    }

    fn compare_set_many(&self, mutations: Vec<BrokerMutation>) -> Result<Vec<Value>, MemoryError> {
        let id = format!(
            "truss-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let request = BrokerRequest {
            protocol_version: PROTOCOL_VERSION,
            id: id.clone(),
            operation: "compare_set_many",
            scope: Some(MEMORY_SCOPE),
            key: None,
            value: None,
            expected_content_hash: None,
            mutations: Some(mutations),
        };
        let response = exchange(&self.socket_path, self.timeout, &request)?;
        if response.protocol_version != PROTOCOL_VERSION || response.id != id {
            return Err(backend("invalid ai-memory broker response correlation"));
        }
        if !response.ok {
            let failure = response
                .error
                .ok_or_else(|| backend("ai-memory broker returned an invalid error"))?;
            return Err(backend(format!(
                "ai-memory broker {}: {}",
                failure.code, failure.message
            )));
        }
        serde_json::from_value(response.result.unwrap_or(Value::Null)).map_err(MemoryError::Json)
    }

    fn get_wire(&self, id: &MemoryId) -> Result<Option<CanonicalTrussEntry>, MemoryError> {
        let key = memory_key(id);
        let value: Option<Value> =
            self.request("get", Some(MEMORY_SCOPE), Some(&key), None, None)?;
        value.map(parse_entry).transpose()
    }

    fn get_versioned(
        &self,
        id: &MemoryId,
    ) -> Result<Option<(String, CanonicalTrussEntry)>, MemoryError> {
        let key = memory_key(id);
        let value: Option<VersionedValue> =
            self.request("get_versioned", Some(MEMORY_SCOPE), Some(&key), None, None)?;
        value
            .map(|versioned| Ok((versioned.content_hash, parse_entry(versioned.value)?)))
            .transpose()
    }

    fn list_wires(&self) -> Result<Vec<CanonicalTrussEntry>, MemoryError> {
        let values: Vec<Value> = self.request("list", Some(MEMORY_SCOPE), None, None, None)?;
        let mut entries = Vec::new();
        for value in values {
            if value.get("adapter").and_then(Value::as_str) != Some("truss") {
                continue;
            }
            entries.push(parse_entry(value)?);
        }
        Ok(entries)
    }

    fn list_entries(&self) -> Result<Vec<MemoryEntry>, MemoryError> {
        self.list_wires()
            .map(|entries| entries.into_iter().map(MemoryEntry::from).collect())
    }

    fn transition(
        &self,
        id: &MemoryId,
        target: Trust,
        verdict: Option<Verdict>,
        verdict_ref: Option<LedgerRef>,
        superseded_by: Option<MemoryId>,
    ) -> Result<(), MemoryError> {
        let (hash, mut wire) = self
            .get_versioned(id)?
            .ok_or_else(|| MemoryError::NotFound(id.clone()))?;
        if !apply_transition(&mut wire, target, verdict, verdict_ref, superseded_by)? {
            return Ok(());
        }
        let key = memory_key(id);
        let predecessor = if matches!(target, Trust::Verified | Trust::Rejected) {
            self.list_wires()?
                .into_iter()
                .find(|entry| entry.superseded_by.as_ref() == Some(id))
        } else {
            None
        };
        if let Some(predecessor) = predecessor {
            let (prior_hash, mut prior) = self
                .get_versioned(&predecessor.id)?
                .ok_or_else(|| MemoryError::NotFound(predecessor.id.clone()))?;
            if prior.superseded_by.as_ref() != Some(id) {
                return Err(backend("ai-memory Truss supersede predecessor changed"));
            }
            if target == Trust::Verified && prior.trust == Trust::Verified {
                prior.trust = Trust::Stale;
            } else if target == Trust::Rejected && prior.trust == Trust::Verified {
                prior.superseded_by = None;
            }
            self.compare_set_many(vec![
                BrokerMutation {
                    key,
                    value: serde_json::to_value(wire)?,
                    expected_content_hash: Some(hash),
                },
                BrokerMutation {
                    key: memory_key(&prior.id),
                    value: serde_json::to_value(prior)?,
                    expected_content_hash: Some(prior_hash),
                },
            ])?;
        } else {
            let _: Value = self.request(
                "compare_set",
                Some(MEMORY_SCOPE),
                Some(&key),
                Some(serde_json::to_value(wire)?),
                Some(&hash),
            )?;
        }
        Ok(())
    }
}

impl MemoryStore for AiMemoryStore {
    fn write(&self, draft: MemoryDraft) -> Result<MemoryId, MemoryError> {
        let id = truss_memory_id(&draft.kind, draft.key.as_deref(), &draft.body);
        if self.get_wire(&id)?.is_some() {
            return Ok(id);
        }
        let source_record_ids = std::iter::once(&draft.produced_by)
            .chain(draft.sources.iter())
            .map(ledger_source_id)
            .collect();
        let wire = CanonicalTrussEntry {
            adapter: "truss".into(),
            schema_version: 1,
            id: id.clone(),
            kind: draft.kind,
            key: draft.key,
            content: draft.body,
            trust: Trust::Unverified,
            provenance: Provenance {
                produced_by: draft.produced_by,
                verified_by: None,
                verified_by_ref: None,
                sources: draft.sources,
            },
            salience: 0.0,
            created_ms: now_ms(),
            superseded_by: None,
            source_record_ids,
        };
        let key = memory_key(&id);
        let predecessor = if should_supersede(wire.kind, wire.key.as_deref()) {
            self.list_wires()?
                .into_iter()
                .filter(|entry| {
                    entry.kind == wire.kind
                        && entry.key == wire.key
                        && entry.trust != Trust::Rejected
                        && entry.superseded_by.is_none()
                })
                .max_by(|left, right| {
                    left.created_ms
                        .cmp(&right.created_ms)
                        .then_with(|| right.id.0.cmp(&left.id.0))
                })
        } else {
            None
        };
        let mut mutations = Vec::new();
        if let Some(predecessor) = predecessor {
            let (prior_hash, mut prior) = self
                .get_versioned(&predecessor.id)?
                .ok_or_else(|| MemoryError::NotFound(predecessor.id.clone()))?;
            if prior.superseded_by.is_some() {
                return Err(backend("ai-memory Truss active predecessor changed"));
            }
            prior.superseded_by = Some(id.clone());
            mutations.push(BrokerMutation {
                key: memory_key(&prior.id),
                value: serde_json::to_value(prior)?,
                expected_content_hash: Some(prior_hash),
            });
        }
        mutations.push(BrokerMutation {
            key,
            value: serde_json::to_value(wire)?,
            expected_content_hash: None,
        });
        self.compare_set_many(mutations)?;
        Ok(id)
    }

    fn promote(&self, id: &MemoryId, verdict: Verdict) -> Result<(), MemoryError> {
        self.transition(id, Trust::Verified, Some(verdict), None, None)
    }

    fn promote_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: Verdict,
        verdict_ref: LedgerRef,
    ) -> Result<(), MemoryError> {
        self.transition(id, Trust::Verified, Some(verdict), Some(verdict_ref), None)
    }

    fn reject(&self, id: &MemoryId, verdict: Verdict) -> Result<(), MemoryError> {
        self.transition(id, Trust::Rejected, Some(verdict), None, None)
    }

    fn reject_with_verdict_ref(
        &self,
        id: &MemoryId,
        verdict: Verdict,
        verdict_ref: LedgerRef,
    ) -> Result<(), MemoryError> {
        self.transition(id, Trust::Rejected, Some(verdict), Some(verdict_ref), None)
    }

    fn mark_stale(
        &self,
        id: &MemoryId,
        superseded_by: Option<MemoryId>,
    ) -> Result<(), MemoryError> {
        self.transition(id, Trust::Stale, None, None, superseded_by)
    }

    fn get(&self, id: &MemoryId) -> Result<MemoryEntry, MemoryError> {
        self.get_wire(id)?
            .map(MemoryEntry::from)
            .ok_or_else(|| MemoryError::NotFound(id.clone()))
    }

    fn query(&self, q: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        if q.limit == 0 {
            return Ok(Vec::new());
        }
        let mut entries = self
            .list_entries()?
            .into_iter()
            .filter(|entry| entry.superseded_by.is_none())
            .filter(|entry| matches_query(entry, q))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .salience
                .total_cmp(&left.salience)
                .then_with(|| right.created_ms.cmp(&left.created_ms))
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        entries.truncate(q.limit);
        Ok(entries)
    }

    fn delta_since(&self, _cursor: MemoryCursor) -> Result<MemoryDelta, MemoryError> {
        // Conservative correctness until the broker exposes a durable global
        // mutation cursor: return the full accessible projection every time.
        Ok(MemoryDelta {
            entries: self.list_entries()?,
            cursor: MemoryCursor(0),
        })
    }

    fn write_policy(&self, draft: PolicyDraft) -> Result<PolicyId, MemoryError> {
        let mut hash = Sha256::new();
        hash.update(draft.policy.schema_version.to_le_bytes());
        hash.update(serde_json::to_vec(&draft.policy)?);
        let id = PolicyId(hex::encode(hash.finalize()));
        let source_record_ids = std::iter::once(&draft.produced_by)
            .chain(draft.sources.iter())
            .map(ledger_source_id)
            .collect();
        let record = PolicyRecord {
            id: id.clone(),
            policy: draft.policy,
            trust: Trust::Unverified,
            provenance: Provenance {
                produced_by: draft.produced_by,
                verified_by: None,
                verified_by_ref: None,
                sources: draft.sources,
            },
            created_ms: now_ms(),
            superseded_by: None,
        };
        let wire = CanonicalTrussPolicy {
            adapter: "truss_policy".into(),
            schema_version: 1,
            record,
            source_record_ids,
        };
        let key = policy_key(&id);
        let _: Value = self.request(
            "set",
            Some(MEMORY_SCOPE),
            Some(&key),
            Some(serde_json::to_value(wire)?),
            None,
        )?;
        Ok(id)
    }

    fn get_policy(&self, id: &PolicyId) -> Result<PolicyRecord, MemoryError> {
        let key = policy_key(id);
        let value: Option<CanonicalTrussPolicy> =
            self.request("get", Some(MEMORY_SCOPE), Some(&key), None, None)?;
        value
            .filter(|wire| wire.adapter == "truss_policy" && wire.schema_version == 1)
            .map(|wire| wire.record)
            .ok_or_else(|| backend(format!("Truss policy record not found: {}", id.0)))
    }
}

impl From<CanonicalTrussEntry> for MemoryEntry {
    fn from(wire: CanonicalTrussEntry) -> Self {
        Self {
            id: wire.id,
            kind: wire.kind,
            key: wire.key,
            body: wire.content,
            trust: wire.trust,
            provenance: wire.provenance,
            salience: wire.salience,
            created_ms: wire.created_ms,
            superseded_by: wire.superseded_by,
        }
    }
}

fn parse_entry(value: Value) -> Result<CanonicalTrussEntry, MemoryError> {
    let wire: CanonicalTrussEntry = serde_json::from_value(value)?;
    if wire.adapter != "truss" || wire.schema_version != 1 {
        return Err(backend("invalid canonical Truss memory record"));
    }
    Ok(wire)
}

fn apply_transition(
    wire: &mut CanonicalTrussEntry,
    target: Trust,
    verdict: Option<Verdict>,
    verdict_ref: Option<LedgerRef>,
    superseded_by: Option<MemoryId>,
) -> Result<bool, MemoryError> {
    let mut changed = false;
    if wire.trust != target {
        match target {
            Trust::Verified | Trust::Rejected if wire.trust != Trust::Unverified => {
                return Err(backend("invalid ai-memory Truss trust transition"));
            }
            Trust::Stale if wire.trust != Trust::Verified => {
                return Err(backend("only verified ai-memory entries may become stale"));
            }
            _ => {}
        }
        wire.trust = target;
        changed = true;
    }

    if wire.provenance.verified_by.is_none() {
        if let Some(verdict) = verdict {
            wire.provenance.verified_by = Some(verdict);
            changed = true;
        }
    }
    if wire.provenance.verified_by_ref.is_none() {
        if let Some(verdict_ref) = verdict_ref {
            wire.provenance.verified_by_ref = Some(verdict_ref);
            changed = true;
        }
    }
    if wire.superseded_by.is_none() {
        if let Some(superseded_by) = superseded_by {
            wire.superseded_by = Some(superseded_by);
            changed = true;
        }
    }
    Ok(changed)
}

fn matches_query(entry: &MemoryEntry, q: &MemoryQuery) -> bool {
    if entry.trust == Trust::Rejected || entry.trust.rank() < q.min_trust.rank() {
        return false;
    }
    if q.kind.is_some_and(|kind| kind != entry.kind) {
        return false;
    }
    if q.key
        .as_ref()
        .is_some_and(|key| entry.key.as_ref() != Some(key))
    {
        return false;
    }
    if q.contains
        .as_ref()
        .is_some_and(|needle| !entry.body.to_lowercase().contains(&needle.to_lowercase()))
    {
        return false;
    }
    true
}

fn should_supersede(kind: EntryKind, key: Option<&str>) -> bool {
    matches!(
        kind,
        EntryKind::Fact | EntryKind::Lesson | EntryKind::Playbook
    ) && key.is_some()
}

fn truss_memory_id(kind: &EntryKind, key: Option<&str>, body: &str) -> MemoryId {
    let mut hash = Sha256::new();
    hash.update(kind_name(*kind).as_bytes());
    hash.update(key.unwrap_or_default().as_bytes());
    hash.update(body.as_bytes());
    MemoryId(hex::encode(hash.finalize()))
}

fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Goal => "goal",
        EntryKind::Decision => "decision",
        EntryKind::Fact => "fact",
        EntryKind::Lesson => "lesson",
        EntryKind::Playbook => "playbook",
        EntryKind::Artifact => "artifact",
        EntryKind::Scratch => "scratch",
        EntryKind::ContextSummary => "context_summary",
    }
}

fn memory_key(id: &MemoryId) -> String {
    format!("truss:{}", id.0)
}

fn policy_key(id: &PolicyId) -> String {
    format!("truss-policy:{}", id.0)
}

fn ledger_source_id(reference: &LedgerRef) -> String {
    format!("truss-ledger\0{}\0{}", reference.session, reference.seq)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn backend(message: impl Into<String>) -> MemoryError {
    MemoryError::Backend(anyhow!(message.into()))
}

#[cfg(unix)]
fn exchange(
    socket_path: &Path,
    timeout: Duration,
    request: &BrokerRequest<'_>,
) -> Result<BrokerResponse, MemoryError> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|error| backend(format!("ai-memory broker connection failed: {error}")))?;
    stream.set_read_timeout(Some(timeout)).map_err(|error| {
        backend(format!(
            "ai-memory broker read timeout setup failed: {error}"
        ))
    })?;
    stream.set_write_timeout(Some(timeout)).map_err(|error| {
        backend(format!(
            "ai-memory broker write timeout setup failed: {error}"
        ))
    })?;
    serde_json::to_writer(&mut stream, request)?;
    stream
        .write_all(b"\n")
        .map_err(|error| backend(format!("ai-memory broker write failed: {error}")))?;
    let mut source = String::new();
    BufReader::new(stream)
        .take(MAX_RESPONSE_BYTES + 1)
        .read_line(&mut source)
        .map_err(|error| backend(format!("ai-memory broker read failed: {error}")))?;
    if source.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(backend("ai-memory broker response exceeds size limit"));
    }
    if !source.ends_with('\n') {
        return Err(backend("ai-memory broker returned an incomplete response"));
    }
    serde_json::from_str(&source).map_err(MemoryError::Json)
}

#[cfg(not(unix))]
fn exchange(
    _socket_path: &Path,
    _timeout: Duration,
    _request: &BrokerRequest<'_>,
) -> Result<BrokerResponse, MemoryError> {
    Err(backend(
        "ai-memory Unix broker backend is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(trust: Trust) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId("one".into()),
            kind: EntryKind::Lesson,
            key: Some("atomic-transitions".into()),
            body: "Use compare-and-set for trust transitions.".into(),
            trust,
            provenance: Provenance {
                produced_by: LedgerRef {
                    session: "session-1".into(),
                    seq: 4,
                },
                verified_by: None,
                verified_by_ref: None,
                sources: Vec::new(),
            },
            salience: 0.5,
            created_ms: 1,
            superseded_by: None,
        }
    }

    #[test]
    fn content_address_matches_truss_sqlite_store() {
        let id = truss_memory_id(
            &EntryKind::Fact,
            Some("stable-key"),
            "Remember the canonical vault.",
        );
        assert_eq!(
            id.0,
            "5e12b18aa1da8d761660bbf6ecdf6dae31109195fb3edc60691b6c6f022da25f"
        );
    }

    #[test]
    fn query_defaults_preserve_trust_boundary() {
        assert!(matches_query(
            &entry(Trust::Verified),
            &MemoryQuery::default()
        ));
        assert!(!matches_query(
            &entry(Trust::Unverified),
            &MemoryQuery::default()
        ));
        assert!(!matches_query(
            &entry(Trust::Rejected),
            &MemoryQuery {
                min_trust: Trust::Unverified,
                ..MemoryQuery::default()
            }
        ));
    }

    #[test]
    fn constructor_requires_absolute_socket_and_positive_timeout() {
        assert!(AiMemoryStore::new("relative.sock").is_err());
        assert!(AiMemoryStore::with_timeout("/tmp/memory.sock", Duration::ZERO).is_err());
        assert!(AiMemoryStore::new("/tmp/memory.sock").is_ok());
    }

    #[test]
    fn batch_create_precondition_serializes_as_explicit_null() {
        let mutation = BrokerMutation {
            key: "truss:new".into(),
            value: serde_json::json!({ "trust": "unverified" }),
            expected_content_hash: None,
        };
        let value = serde_json::to_value(mutation).unwrap();
        assert!(value.get("expectedContentHash").is_some());
        assert!(value["expectedContentHash"].is_null());
    }

    #[test]
    fn only_keyed_fact_lesson_and_playbook_entries_supersede() {
        assert!(should_supersede(EntryKind::Fact, Some("key")));
        assert!(should_supersede(EntryKind::Lesson, Some("key")));
        assert!(should_supersede(EntryKind::Playbook, Some("key")));
        assert!(!should_supersede(EntryKind::Decision, Some("key")));
        assert!(!should_supersede(EntryKind::Fact, None));
    }

    #[test]
    fn canonical_wire_matches_the_existing_ai_memory_adapter_schema() {
        let wire = CanonicalTrussEntry {
            adapter: "truss".into(),
            schema_version: 1,
            id: MemoryId("entry-1".into()),
            kind: EntryKind::Fact,
            key: Some("wire-schema".into()),
            content: "Keep the shared schema stable.".into(),
            trust: Trust::Unverified,
            provenance: Provenance {
                produced_by: LedgerRef {
                    session: "session-1".into(),
                    seq: 3,
                },
                verified_by: None,
                verified_by_ref: None,
                sources: Vec::new(),
            },
            salience: 0.0,
            created_ms: 42,
            superseded_by: Some(MemoryId("entry-2".into())),
            source_record_ids: vec![ledger_source_id(&LedgerRef {
                session: "session-1".into(),
                seq: 3,
            })],
        };
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["created_ms"], 42);
        assert_eq!(value["superseded_by"], "entry-2");
        assert!(value.get("sourceRecordIds").is_some());
        assert!(value.get("schemaVersion").is_none());
        assert_eq!(
            serde_json::from_value::<CanonicalTrussEntry>(value)
                .unwrap()
                .id,
            wire.id
        );
    }

    #[test]
    fn stale_transition_preserves_signed_verdict_provenance() {
        let verdict_ref = LedgerRef {
            session: "verdict-session".into(),
            seq: 9,
        };
        let mut wire = CanonicalTrussEntry {
            adapter: "truss".into(),
            schema_version: 1,
            id: MemoryId("entry-1".into()),
            kind: EntryKind::Fact,
            key: Some("provenance".into()),
            content: "Preserve signed evidence.".into(),
            trust: Trust::Verified,
            provenance: Provenance {
                produced_by: LedgerRef {
                    session: "session-1".into(),
                    seq: 3,
                },
                verified_by: None,
                verified_by_ref: Some(verdict_ref.clone()),
                sources: Vec::new(),
            },
            salience: 0.0,
            created_ms: 42,
            superseded_by: None,
            source_record_ids: Vec::new(),
        };

        assert!(apply_transition(&mut wire, Trust::Stale, None, None, None).unwrap());
        assert_eq!(wire.trust, Trust::Stale);
        assert_eq!(wire.provenance.verified_by_ref, Some(verdict_ref));
    }

    #[test]
    fn idempotent_transition_can_backfill_missing_verdict_reference() {
        let mut wire = CanonicalTrussEntry {
            adapter: "truss".into(),
            schema_version: 1,
            id: MemoryId("entry-1".into()),
            kind: EntryKind::Fact,
            key: Some("provenance".into()),
            content: "Backfill signed evidence.".into(),
            trust: Trust::Verified,
            provenance: Provenance {
                produced_by: LedgerRef {
                    session: "session-1".into(),
                    seq: 3,
                },
                verified_by: None,
                verified_by_ref: None,
                sources: Vec::new(),
            },
            salience: 0.0,
            created_ms: 42,
            superseded_by: None,
            source_record_ids: Vec::new(),
        };
        let verdict_ref = LedgerRef {
            session: "verdict-session".into(),
            seq: 9,
        };

        assert!(apply_transition(
            &mut wire,
            Trust::Verified,
            None,
            Some(verdict_ref.clone()),
            None,
        )
        .unwrap());
        assert_eq!(wire.provenance.verified_by_ref, Some(verdict_ref));
    }

    #[cfg(unix)]
    #[test]
    fn broker_request_has_version_and_no_identity_claim() {
        use std::fs;
        use std::os::unix::net::UnixListener;
        use std::thread;

        let root = std::env::temp_dir().join(format!(
            "truss-ai-memory-test-{}-{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let socket_path = root.join("broker.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["protocolVersion"], 1);
            assert_eq!(request["operation"], "get");
            assert!(request.get("principal").is_none());
            let response = serde_json::json!({
                "protocolVersion": 1,
                "id": request["id"],
                "ok": true,
                "result": null
            });
            let mut writer = stream;
            writeln!(writer, "{response}").unwrap();
        });

        let store = AiMemoryStore::new(&socket_path).unwrap();
        let result = store.get(&MemoryId("missing".into()));
        assert!(matches!(result, Err(MemoryError::NotFound(_))));
        server.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    /// Cross-repository canary for a real ai-memory daemon. Run explicitly:
    /// `TRUSS_AI_MEMORY_CANARY_SOCKET=/absolute/socket cargo test -p truss-core
    /// real_ai_memory_broker_canary -- --ignored`
    #[cfg(unix)]
    #[test]
    #[ignore = "requires a separately started ai-memory broker"]
    fn real_ai_memory_broker_canary() {
        let socket = std::env::var_os("TRUSS_AI_MEMORY_CANARY_SOCKET")
            .expect("TRUSS_AI_MEMORY_CANARY_SOCKET is required");
        let store = AiMemoryStore::new(PathBuf::from(socket)).unwrap();
        let body = format!("Truss real-broker canary {}", std::process::id());
        let id = store
            .write(MemoryDraft {
                kind: EntryKind::Decision,
                key: None,
                body,
                produced_by: LedgerRef {
                    session: format!("canary-{}", std::process::id()),
                    seq: 1,
                },
                sources: Vec::new(),
            })
            .unwrap();
        let written = store.get(&id).unwrap();
        assert_eq!(written.trust, Trust::Unverified);
        assert_eq!(written.id, id);
    }
}
