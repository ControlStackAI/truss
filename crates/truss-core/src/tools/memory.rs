//! Native `memory_20250818` tool binding.
//!
//! M2 keeps the store behind the regular tool registry while preserving the M1
//! invariant: model-authored writes land `Unverified` and only default recalls
//! surface `Verified` entries.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::Tool;
use crate::ctx::Ctx;
use crate::delegation::{self, ClauseResult, ClauseVerdict, Verdict};
use crate::error::ToolError;
use crate::events::AgentEvent;
use crate::memory::{
    EntryKind, LedgerRef, MemoryConfig, MemoryDraft, MemoryEntry, MemoryId, MemoryQuery,
    MemoryStore, Trust,
};
use crate::provider::{Provider, ToolDef};
use crate::store::{tool_call_key, Store};

const MEMORY_TOOL_TYPE: &str = "memory_20250818";

#[derive(Clone)]
pub struct MemoryReadJournal {
    pub store: Store,
    pub session_id: String,
}

#[derive(Clone, Copy)]
enum MemoryReadToolKind {
    Query,
    View,
}

pub struct MemoryReadTool {
    kind: MemoryReadToolKind,
    store: Arc<dyn MemoryStore>,
    journal: Option<MemoryReadJournal>,
    recall: crate::memory::MemoryRecallMode,
}

struct MemoryReadCallRecord<'a> {
    produced_by: Option<LedgerRef>,
    memo_key: &'a str,
    input_hash: &'a str,
    result: &'a str,
    is_error: bool,
    error: Option<&'a str>,
}

impl MemoryReadTool {
    fn new(
        kind: MemoryReadToolKind,
        store: Arc<dyn MemoryStore>,
        journal: Option<MemoryReadJournal>,
        recall: crate::memory::MemoryRecallMode,
    ) -> Self {
        Self {
            kind,
            store,
            journal,
            recall,
        }
    }

    pub fn query(
        store: Arc<dyn MemoryStore>,
        journal: Option<MemoryReadJournal>,
        recall: crate::memory::MemoryRecallMode,
    ) -> Self {
        Self::new(MemoryReadToolKind::Query, store, journal, recall)
    }

    pub fn view(
        store: Arc<dyn MemoryStore>,
        journal: Option<MemoryReadJournal>,
        recall: crate::memory::MemoryRecallMode,
    ) -> Self {
        Self::new(MemoryReadToolKind::View, store, journal, recall)
    }

    fn name(&self) -> &'static str {
        match self.kind {
            MemoryReadToolKind::Query => "memory_query",
            MemoryReadToolKind::View => "memory_view",
        }
    }

    async fn call_inner(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let revalidate = input
            .get("revalidate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let memo_input = memo_input(&input);
        let input_hash = sha256_json(&memo_input);
        let memo_key = tool_call_key(self.name(), &memo_input);

        if !revalidate {
            if let Some(journal) = &self.journal {
                if let Some((content, is_error)) = journal
                    .store
                    .memo_get(&journal.session_id, &memo_key)
                    .map_err(|e| ToolError::Execution(format!("read memory memo: {e}")))?
                {
                    self.verify_memo(journal, &memo_key, &input_hash, &content, is_error)?;
                    if is_error {
                        return Err(ToolError::Execution(content));
                    }
                    return Ok(content);
                }
            }
        }

        let result = match self.kind {
            MemoryReadToolKind::Query => self.query_memory(ctx, &input),
            MemoryReadToolKind::View => self.view_memory(ctx, &input),
        };
        match result {
            Ok(out) => {
                self.record_provenance(
                    ctx,
                    MemoryReadCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &out,
                        is_error: false,
                        error: None,
                    },
                )?;
                Ok(out)
            }
            Err(err) => {
                let err_text = err.to_string();
                self.record_provenance(
                    ctx,
                    MemoryReadCallRecord {
                        produced_by,
                        memo_key: &memo_key,
                        input_hash: &input_hash,
                        result: &err_text,
                        is_error: true,
                        error: Some(&err_text),
                    },
                )?;
                Err(err)
            }
        }
    }

    fn query_memory(&self, ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        let contains = input_str(input, &["query", "contains"]).map(str::to_string);
        let kind = optional_kind(input)?;
        let key = input_str(input, &["key"]).map(str::to_string);
        let min_trust = optional_trust(input)?.unwrap_or(Trust::Verified);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 100) as usize)
            .unwrap_or(32);
        let query = MemoryQuery {
            kind,
            key,
            min_trust,
            contains,
            limit,
        };
        let entries = self.store.query(&query).map_err(memory_error)?;
        ctx.emit(AgentEvent::MemoryRecall {
            path: "memory_query".into(),
            kind,
            min_trust,
            count: entries.len(),
        });
        render_json(json!({
            "tool": self.name(),
            "recall_mode": recall_label(self.recall),
            "min_trust": trust_label(min_trust),
            "count": entries.len(),
            "entries": entries_json(&entries),
        }))
    }

    fn view_memory(&self, ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        let ids = requested_ids(input)?;
        let min_trust = optional_trust(input)?.unwrap_or(Trust::Verified);
        let mut entries = Vec::new();
        let mut filtered = Vec::new();
        let mut missing = Vec::new();
        for id in ids {
            match self.store.get(&MemoryId(id.clone())) {
                Ok(entry)
                    if entry.trust != Trust::Rejected && entry.trust.rank() >= min_trust.rank() =>
                {
                    entries.push(entry);
                }
                Ok(_) => filtered.push(id),
                Err(crate::memory::MemoryError::NotFound(_)) => missing.push(id),
                Err(err) => return Err(memory_error(err)),
            }
        }
        ctx.emit(AgentEvent::MemoryRecall {
            path: "memory_view".into(),
            kind: None,
            min_trust,
            count: entries.len(),
        });
        render_json(json!({
            "tool": self.name(),
            "recall_mode": recall_label(self.recall),
            "min_trust": trust_label(min_trust),
            "count": entries.len(),
            "entries": entries_json(&entries),
            "filtered": filtered,
            "missing": missing,
        }))
    }

    fn record_provenance(
        &self,
        ctx: &Ctx,
        record: MemoryReadCallRecord<'_>,
    ) -> Result<(), ToolError> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        journal
            .store
            .append_with_memo(
                &journal.session_id,
                "memory_tool_call",
                &json!({
                    "tool": self.name(),
                    "memo_key": record.memo_key,
                    "input_sha256": record.input_hash,
                    "result_sha256": sha256_str(record.result),
                    "is_error": record.is_error,
                    "side_effect": false,
                    "produced_by": record.produced_by,
                    "error": record.error,
                }),
                record.memo_key,
                record.result,
                record.is_error,
            )
            .map_err(|e| ToolError::Execution(format!("record memory provenance: {e}")))?;
        Ok(())
    }

    fn verify_memo(
        &self,
        journal: &MemoryReadJournal,
        memo_key: &str,
        input_hash: &str,
        content: &str,
        is_error: bool,
    ) -> Result<(), ToolError> {
        let result_hash = sha256_str(content);
        let entries = journal
            .store
            .entries(&journal.session_id)
            .map_err(|e| ToolError::Execution(format!("read memory provenance: {e}")))?;
        let verified = entries.iter().rev().any(|entry| {
            entry.kind == "memory_tool_call"
                && entry.payload["tool"].as_str() == Some(self.name())
                && entry.payload["memo_key"].as_str() == Some(memo_key)
                && entry.payload["input_sha256"].as_str() == Some(input_hash)
                && entry.payload["result_sha256"].as_str() == Some(result_hash.as_str())
                && entry.payload["is_error"].as_bool() == Some(is_error)
                && entry.payload["side_effect"].as_bool() == Some(false)
        });
        if verified {
            Ok(())
        } else {
            Err(ToolError::Execution(
                "memory memo missing matching signed provenance".into(),
            ))
        }
    }
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn def(&self) -> ToolDef {
        match self.kind {
            MemoryReadToolKind::Query => ToolDef {
                name: self.name().into(),
                description:
                    "Query L2 durable memory with verified-only recall by default; returns typed entries with full ledger provenance."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Text to match using the configured memory recall mode." },
                        "contains": { "type": "string", "description": "Alias for query." },
                        "kind": { "type": "string", "enum": ["goal", "decision", "fact", "lesson", "playbook", "artifact", "scratch", "context_summary"] },
                        "key": { "type": "string", "description": "Supersede key for keyed memory kinds." },
                        "limit": { "type": "integer", "description": "Maximum entries to return. Defaults to 32, max 100." },
                        "min_trust": { "type": "string", "enum": ["verified", "stale", "unverified"], "description": "Trust floor. Defaults to verified." },
                        "revalidate": { "type": "boolean", "description": "Bypass any recorded memo and query the current store." }
                    }
                }),
            },
            MemoryReadToolKind::View => ToolDef {
                name: self.name().into(),
                description:
                    "Fetch one or more L2 durable memory entries by id with verified-only recall by default and full ledger provenance."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Memory entry id to fetch." },
                        "ids": { "type": "array", "items": { "type": "string" }, "description": "Memory entry ids to fetch." },
                        "min_trust": { "type": "string", "enum": ["verified", "stale", "unverified"], "description": "Trust floor. Defaults to verified." },
                        "revalidate": { "type": "boolean", "description": "Bypass any recorded memo and query the current store." }
                    }
                }),
            },
        }
    }

    async fn call(&self, ctx: &Ctx, input: Value) -> Result<String, ToolError> {
        self.call_inner(ctx, input, None).await
    }

    async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        self.call_inner(ctx, input, produced_by).await
    }
}

pub struct MemoryTool {
    store: Arc<dyn MemoryStore>,
    config: MemoryConfig,
    reviewers: Vec<Arc<dyn Provider>>,
    journal: Option<Store>,
}

impl MemoryTool {
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self::with_promotion(store, MemoryConfig::default(), Vec::new())
    }

    pub fn with_promotion(
        store: Arc<dyn MemoryStore>,
        config: MemoryConfig,
        reviewers: Vec<Arc<dyn Provider>>,
    ) -> Self {
        Self {
            store,
            config,
            reviewers,
            journal: None,
        }
    }

    pub fn with_signed_promotion(
        store: Arc<dyn MemoryStore>,
        config: MemoryConfig,
        reviewers: Vec<Arc<dyn Provider>>,
        journal: Store,
    ) -> Self {
        Self {
            store,
            config,
            reviewers,
            journal: Some(journal),
        }
    }

    async fn execute(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let command = input_str(&input, &["command", "cmd", "operation", "op"])
            .unwrap_or("")
            .to_ascii_lowercase();
        match command.as_str() {
            "create" | "insert" | "str_replace" => {
                self.write(ctx, &input, produced_by, &command).await
            }
            "view" => self.view(ctx, &input).await,
            "delete" => self.delete(ctx, &input, produced_by),
            "rename" => self.rename(ctx, &input, produced_by),
            "" => Err(ToolError::InvalidInput("missing memory command".into())),
            other => Err(ToolError::InvalidInput(format!(
                "unsupported memory command '{other}'"
            ))),
        }
    }

    async fn write(
        &self,
        ctx: &Ctx,
        input: &Value,
        produced_by: Option<LedgerRef>,
        command: &str,
    ) -> Result<String, ToolError> {
        let produced_by = produced_by.ok_or_else(|| {
            ToolError::Execution("memory writes require durable journal provenance".into())
        })?;
        let path = required_str(input, &["path", "file_path"])?;
        let body = write_body(input, command)?;
        let kind = kind_for_path(path);
        let key = key_for_path(path);
        let draft = MemoryDraft {
            kind,
            key: key.clone(),
            body,
            produced_by: produced_by.clone(),
            sources: Vec::new(),
        };
        let id = self.store.write(draft).map_err(memory_error)?;
        ctx.emit(AgentEvent::MemoryWrite {
            id: id.0.clone(),
            path: path.to_string(),
            kind,
            key,
            trust: Trust::Unverified,
            produced_by,
        });
        if matches!(
            self.config.verification,
            crate::memory::VerificationPolicy::Tiered
        ) {
            let entry = self.store.get(&id).map_err(memory_error)?;
            if let Some(verdict) = heuristic_verdict(&entry) {
                self.store.reject(&id, verdict).map_err(memory_error)?;
                return Ok(format!("rejected memory {} by heuristic gate", id.0));
            }
        }
        if self.config.verify_on_write(kind) {
            if let Some(verdict) = self.verify_entry(ctx, &id).await? {
                let overall = verdict.overall.clone();
                self.apply_verdict(&id, verdict)?;
                return Ok(match overall {
                    ClauseVerdict::Pass => format!("promoted verified memory {}", id.0),
                    ClauseVerdict::Fail | ClauseVerdict::Unverifiable => {
                        format!("rejected memory {}", id.0)
                    }
                });
            }
        }
        Ok(format!("stored unverified memory {}", id.0))
    }

    async fn view(&self, ctx: &Ctx, input: &Value) -> Result<String, ToolError> {
        let path = input_str(input, &["path", "file_path"]).unwrap_or("/memories");
        let kind = query_kind_for_path(path);
        let key = key_for_path(path);
        let min_trust = match self.config.verification {
            crate::memory::VerificationPolicy::Off | crate::memory::VerificationPolicy::Tiered => {
                Trust::Unverified
            }
            crate::memory::VerificationPolicy::Lazy => Trust::Unverified,
            crate::memory::VerificationPolicy::Eager => Trust::Verified,
        };
        let entries = self
            .store
            .query(&MemoryQuery {
                kind,
                key,
                min_trust,
                contains: None,
                limit: input
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(32),
            })
            .map_err(memory_error)?;
        let entries = self.verify_recalled(ctx, entries).await?;
        ctx.emit(AgentEvent::MemoryRecall {
            path: path.to_string(),
            kind,
            min_trust,
            count: entries.len(),
        });
        Ok(render_entries(path, &entries))
    }

    async fn verify_recalled(
        &self,
        ctx: &Ctx,
        entries: Vec<MemoryEntry>,
    ) -> Result<Vec<MemoryEntry>, ToolError> {
        let mut surfaced = Vec::new();
        for entry in entries {
            if entry.trust == Trust::Stale {
                continue;
            }
            match self.config.verification {
                crate::memory::VerificationPolicy::Lazy if entry.trust == Trust::Unverified => {
                    if let Some(verdict) = self.verify_entry(ctx, &entry.id).await? {
                        let passed = verdict.overall == ClauseVerdict::Pass;
                        self.apply_verdict(&entry.id, verdict)?;
                        if passed {
                            surfaced.push(self.store.get(&entry.id).map_err(memory_error)?);
                        }
                    }
                }
                crate::memory::VerificationPolicy::Eager if entry.trust != Trust::Verified => {}
                _ => surfaced.push(entry),
            }
        }
        Ok(surfaced)
    }

    async fn verify_entry(&self, ctx: &Ctx, id: &MemoryId) -> Result<Option<Verdict>, ToolError> {
        if self.reviewers.is_empty() {
            return Ok(None);
        }
        let entry = self.store.get(id).map_err(memory_error)?;
        Ok(Some(
            delegation::run_memory_claim(&self.reviewers, ctx, &entry).await,
        ))
    }

    fn apply_verdict(&self, id: &MemoryId, verdict: Verdict) -> Result<(), ToolError> {
        delegation::apply_memory_verdict(&*self.store, self.journal.as_ref(), id, verdict)
            .map_err(|e| ToolError::Execution(e.to_string()))
    }

    fn delete(
        &self,
        ctx: &Ctx,
        input: &Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let produced_by = produced_by.ok_or_else(|| {
            ToolError::Execution("memory delete requires durable journal provenance".into())
        })?;
        let path = required_str(input, &["path", "file_path"])?;
        let ids = active_ids(&*self.store, path)?;
        for id in &ids {
            self.store.mark_stale(id, None).map_err(memory_error)?;
        }
        ctx.emit(AgentEvent::MemoryWrite {
            id: ids
                .first()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "<none>".into()),
            path: path.to_string(),
            kind: kind_for_path(path),
            key: key_for_path(path),
            trust: Trust::Stale,
            produced_by,
        });
        Ok(format!("marked {} memory entrie(s) stale", ids.len()))
    }

    fn rename(
        &self,
        ctx: &Ctx,
        input: &Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        let produced_by = produced_by.ok_or_else(|| {
            ToolError::Execution("memory rename requires durable journal provenance".into())
        })?;
        let from = required_str(input, &["path", "file_path", "from"])?;
        let to = required_str(input, &["new_path", "to", "destination"])?;
        let entries = active_entries(&*self.store, from)?;
        let mut new_ids = Vec::new();
        for entry in entries {
            let draft = MemoryDraft {
                kind: kind_for_path(to),
                key: key_for_path(to),
                body: entry.body,
                produced_by: produced_by.clone(),
                sources: Vec::new(),
            };
            let new_id = self.store.write(draft).map_err(memory_error)?;
            self.store
                .mark_stale(&entry.id, Some(new_id.clone()))
                .map_err(memory_error)?;
            new_ids.push(new_id);
        }
        ctx.emit(AgentEvent::MemoryWrite {
            id: new_ids
                .first()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "<none>".into()),
            path: to.to_string(),
            kind: kind_for_path(to),
            key: key_for_path(to),
            trust: Trust::Unverified,
            produced_by,
        });
        Ok(format!("renamed {} memory entrie(s)", new_ids.len()))
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "memory".into(),
            description: "Read and write governed Truss memory.".into(),
            // Providers that support Anthropic native tools translate this
            // sentinel into `type: memory_20250818`; others can treat it as an
            // ordinary tool schema if explicitly wired later.
            input_schema: json!({ "type": MEMORY_TOOL_TYPE }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: Value) -> Result<String, ToolError> {
        self.execute(ctx, input, None).await
    }

    async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        input: Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        self.execute(ctx, input, produced_by).await
    }
}

pub fn kind_for_path(path: &str) -> EntryKind {
    let p = path.trim_start_matches('/');
    let p = p.strip_prefix("memories/").unwrap_or(p);
    if p.starts_with("goal/") || p.starts_with("goals/") {
        EntryKind::Goal
    } else if p.starts_with("decisions/") || p.starts_with("decision/") {
        EntryKind::Decision
    } else if p.starts_with("lessons/") || p.starts_with("lesson/") {
        EntryKind::Lesson
    } else if p.starts_with("playbooks/") || p.starts_with("playbook/") {
        EntryKind::Playbook
    } else if p.starts_with("facts/") || p.starts_with("fact/") {
        EntryKind::Fact
    } else if p.starts_with("artifacts/") || p.starts_with("artifact/") {
        EntryKind::Artifact
    } else {
        EntryKind::Scratch
    }
}

fn query_kind_for_path(path: &str) -> Option<EntryKind> {
    let kind = kind_for_path(path);
    if kind == EntryKind::Scratch && key_for_path(path).is_none() {
        None
    } else {
        Some(kind)
    }
}

fn key_for_path(path: &str) -> Option<String> {
    let p = path.trim_matches('/');
    let p = p.strip_prefix("memories/").unwrap_or(p);
    let mut parts = p.split('/').filter(|s| !s.is_empty());
    let first = parts.next()?;
    let last = parts.next_back().unwrap_or(first);
    if is_kind_dir(last) {
        None
    } else {
        Some(last.to_string())
    }
}

fn is_kind_dir(s: &str) -> bool {
    matches!(
        s,
        "memories"
            | "goal"
            | "goals"
            | "decision"
            | "decisions"
            | "lesson"
            | "lessons"
            | "playbook"
            | "playbooks"
            | "fact"
            | "facts"
            | "artifact"
            | "artifacts"
    )
}

fn optional_kind(input: &Value) -> Result<Option<EntryKind>, ToolError> {
    input
        .get("kind")
        .and_then(Value::as_str)
        .map(parse_kind)
        .transpose()
}

fn parse_kind(kind: &str) -> Result<EntryKind, ToolError> {
    match kind {
        "goal" => Ok(EntryKind::Goal),
        "decision" => Ok(EntryKind::Decision),
        "fact" => Ok(EntryKind::Fact),
        "lesson" => Ok(EntryKind::Lesson),
        "playbook" => Ok(EntryKind::Playbook),
        "artifact" => Ok(EntryKind::Artifact),
        "scratch" => Ok(EntryKind::Scratch),
        "context_summary" => Ok(EntryKind::ContextSummary),
        other => Err(ToolError::InvalidInput(format!(
            "unsupported memory kind '{other}'"
        ))),
    }
}

fn optional_trust(input: &Value) -> Result<Option<Trust>, ToolError> {
    input
        .get("min_trust")
        .and_then(Value::as_str)
        .map(parse_trust)
        .transpose()
}

fn parse_trust(trust: &str) -> Result<Trust, ToolError> {
    match trust {
        "verified" => Ok(Trust::Verified),
        "stale" => Ok(Trust::Stale),
        "unverified" => Ok(Trust::Unverified),
        "rejected" => Err(ToolError::InvalidInput(
            "rejected memory is never surfaced by native recall tools".into(),
        )),
        other => Err(ToolError::InvalidInput(format!(
            "unsupported memory trust tier '{other}'"
        ))),
    }
}

fn requested_ids(input: &Value) -> Result<Vec<String>, ToolError> {
    let mut ids = Vec::new();
    if let Some(id) = input.get("id").and_then(Value::as_str) {
        if !id.is_empty() {
            ids.push(id.to_string());
        }
    }
    if let Some(values) = input.get("ids").and_then(Value::as_array) {
        for value in values {
            let id = value
                .as_str()
                .ok_or_else(|| ToolError::InvalidInput("ids must contain strings".into()))?;
            if !id.is_empty() {
                ids.push(id.to_string());
            }
        }
    }
    if ids.is_empty() {
        return Err(ToolError::InvalidInput("missing memory id".into()));
    }
    Ok(ids)
}

fn required_str<'a>(input: &'a Value, keys: &[&str]) -> Result<&'a str, ToolError> {
    input_str(input, keys)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidInput(format!("missing {}", keys[0])))
}

fn input_str<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
}

fn write_body(input: &Value, command: &str) -> Result<String, ToolError> {
    let keys: &[&str] = if command == "str_replace" {
        &["new_str", "content", "text", "body"]
    } else {
        &["content", "text", "body", "file_text", "new_str"]
    };
    required_str(input, keys).map(str::to_string)
}

fn active_ids(store: &dyn MemoryStore, path: &str) -> Result<Vec<MemoryId>, ToolError> {
    Ok(active_entries(store, path)?
        .into_iter()
        .map(|entry| entry.id)
        .collect())
}

fn active_entries(
    store: &dyn MemoryStore,
    path: &str,
) -> Result<Vec<crate::memory::MemoryEntry>, ToolError> {
    store
        .query(&MemoryQuery {
            kind: query_kind_for_path(path),
            key: key_for_path(path),
            min_trust: Trust::Unverified,
            contains: None,
            limit: 64,
        })
        .map_err(memory_error)
}

fn render_entries(path: &str, entries: &[crate::memory::MemoryEntry]) -> String {
    if entries.is_empty() {
        return format!("{path}\n  (no memories)");
    }

    let mut out = String::new();
    out.push_str(path);
    out.push('\n');
    for entry in entries {
        let p = render_path(entry.kind, entry.key.as_deref());
        out.push_str("  ");
        out.push_str(&p);
        out.push('\n');
        out.push_str("    trust: ");
        out.push_str(trust_label(entry.trust));
        out.push('\n');
        for line in entry.body.lines() {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn heuristic_verdict(entry: &MemoryEntry) -> Option<Verdict> {
    let body = entry.body.trim();
    let reason = if body.is_empty() {
        Some("memory body is empty")
    } else if body.contains('\0') {
        Some("memory body contains a NUL byte")
    } else {
        None
    }?;
    Some(Verdict {
        contract_id: format!("memory-heuristic-{}", entry.id.0),
        title: "Memory heuristic gate".into(),
        overall: ClauseVerdict::Fail,
        results: vec![ClauseResult {
            kind: "check".into(),
            desc: "cheap memory hygiene gate".into(),
            verdict: ClauseVerdict::Fail,
            evidence: reason.into(),
        }],
        executor_summary: entry.body.clone(),
        reviewed_diff: None,
        usage: Default::default(),
    })
}

fn trust_label(trust: Trust) -> &'static str {
    match trust {
        Trust::Unverified => "unverified",
        Trust::Verified => "verified",
        Trust::Rejected => "rejected",
        Trust::Stale => "stale",
    }
}

fn recall_label(recall: crate::memory::MemoryRecallMode) -> &'static str {
    match recall {
        crate::memory::MemoryRecallMode::Exact => "exact",
        crate::memory::MemoryRecallMode::Semantic => "semantic",
        crate::memory::MemoryRecallMode::Hybrid => "hybrid",
    }
}

fn entries_json(entries: &[MemoryEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.id.0.clone(),
                    "kind": kind_label(entry.kind),
                    "key": entry.key.clone(),
                    "body": entry.body.clone(),
                    "trust": trust_label(entry.trust),
                    "ledger_ref": entry.provenance.produced_by.clone(),
                    "provenance": entry.provenance.clone(),
                    "salience": entry.salience,
                    "created_ms": entry.created_ms,
                    "superseded_by": entry.superseded_by.as_ref().map(|id| id.0.clone()),
                })
            })
            .collect(),
    )
}

fn kind_label(kind: EntryKind) -> &'static str {
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

fn memo_input(input: &Value) -> Value {
    let mut value = input.clone();
    if let Value::Object(map) = &mut value {
        map.remove("revalidate");
    }
    value
}

fn render_json(value: Value) -> Result<String, ToolError> {
    serde_json::to_string_pretty(&value).map_err(|e| ToolError::Execution(e.to_string()))
}

fn sha256_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

fn sha256_str(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(value);
    format!("{:x}", h.finalize())
}

fn render_path(kind: EntryKind, key: Option<&str>) -> String {
    let dir = match kind {
        EntryKind::Goal => "/memories/goal",
        EntryKind::Decision => "/decisions",
        EntryKind::Fact => "/facts",
        EntryKind::Lesson => "/lessons",
        EntryKind::Playbook => "/playbooks",
        EntryKind::Artifact => "/artifacts",
        EntryKind::Scratch => "/scratch",
        EntryKind::ContextSummary => "/context-summaries",
    };
    match key {
        Some(key) => format!("{dir}/{key}"),
        None => dir.to_string(),
    }
}

fn memory_error(err: crate::memory::MemoryError) -> ToolError {
    ToolError::Execution(err.to_string())
}
