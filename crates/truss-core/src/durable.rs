//! Durable agent loop (Stage A — journal-authoritative).
//!
//! Every step is persisted to a [`crate::store::Store`]. On resume the
//! conversation is **reconstructed from the journal** ([`crate::replay`]) rather
//! than restored from a snapshot: recorded model/tool outcomes are *replayed*
//! (re-derived from the log), and the live loop continues from the frontier — the
//! first un-recorded step. The loop's next action is a pure function of the last
//! reconstructed message, so every crash point recovers correctly and no recorded
//! step is ever re-executed.
//!
//! A run can **suspend** (the `wait` timer or `await_signal`) with the process
//! fully exiting, and **resume** with exact context. Snapshots are now optional
//! checkpoints; correctness flows from the journal. See
//! `docs/durable-execution.md`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::approval::{ApprovalGatePolicy, ApprovalGateTrigger, APPROVAL_GATE_SIGNAL};
use crate::context_gauge::{ContextGauge, ContextGrowthEstimator, ContextPressure};
use crate::ctx::Ctx;
use crate::error::HarnessError;
use crate::events::AgentEvent;
use crate::memory::{CompactionMode, EntryKind, LedgerRange, LedgerRef, MemoryDraft};
use crate::message::{Block, Message, Role};
use crate::provider::{CompletionRequest, Provider, ToolDef};
use crate::retry::RetryPolicy;
use crate::store::Store;
use crate::tools::ToolRegistry;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Knobs for a durable run.
#[derive(Clone)]
pub struct DurableConfig {
    pub system: Option<String>,
    pub model: Option<String>,
    pub max_tokens: u32,
    pub max_steps: usize,
    pub reasoning: Option<crate::provider::ReasoningEffort>,
    pub temperature: Option<f32>,
    pub retry: RetryPolicy,
    /// If > 0, project the message history down to Head + summary + this many
    /// most-recent turns before each model call (see [`crate::projection`]).
    pub max_context_msgs: usize,
    /// Advertise and execute the native governed-memory tool for this session.
    /// Defaults off so existing durable runs are unchanged.
    pub memory_tool: bool,
    /// Resolved trust/identity policy for governed memory in this session.
    pub memory: crate::memory::MemoryConfig,
    /// Optional M4 L0 persona block. When set, it is rendered into the cached
    /// system prefix ahead of the existing `system` text.
    pub persona: Option<crate::memory::Persona>,
    /// Optional L1 principal block, cached with L0.
    pub principal: Option<String>,
    /// Optional active contract goal ledger (L3), injected as a message and
    /// refreshed on the configured cadence.
    pub goal_ledger: Option<crate::memory::GoalLedger>,
    /// Last L2 cursor the model has already seen. Resume/re-pin injects only
    /// memory deltas after this cursor.
    pub memory_cursor: crate::memory::MemoryCursor,
    /// Optional governed memory substrate used for verified L2 recall at fresh
    /// run start. This feeds the existing L2 injection path.
    pub memory_store: Option<Arc<dyn crate::memory::MemoryStore>>,
    /// Opt-in human approval gates for high-risk tools or spend thresholds.
    pub approval_gates: ApprovalGatePolicy,
}

impl Default for DurableConfig {
    fn default() -> Self {
        DurableConfig {
            system: None,
            model: None,
            max_tokens: 4096,
            max_steps: 24,
            reasoning: None,
            temperature: None,
            retry: RetryPolicy::default(),
            max_context_msgs: 0,
            memory_tool: false,
            memory: crate::memory::MemoryConfig::default(),
            persona: None,
            principal: None,
            goal_ledger: None,
            memory_cursor: crate::memory::MemoryCursor(0),
            memory_store: None,
            approval_gates: ApprovalGatePolicy::default(),
        }
    }
}

/// The result of advancing a durable run.
#[derive(Debug, Clone)]
pub enum DurableOutcome {
    /// Run reached a terminal stop. `final_text` is the last assistant text.
    Completed { final_text: String },
    /// Run suspended awaiting something (a timer or an external signal).
    Suspended { await_desc: String, reason: String },
    /// Resume was called but the await isn't satisfied yet (timer not elapsed,
    /// or no matching signal delivered).
    StillWaiting { await_desc: String },
}

/// The built-in `wait` tool: suspend on a timer.
pub fn wait_tool_def() -> ToolDef {
    ToolDef {
        name: "wait".into(),
        description: "Suspend the run for a number of seconds (e.g. to wait on an \
                      external event). The process may exit entirely and resume later \
                      with full context."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "seconds": { "type": "integer", "description": "How long to wait, in seconds" },
                "reason": { "type": "string", "description": "Why the run is waiting" }
            },
            "required": ["seconds"]
        }),
    }
}

/// The built-in `await_signal` tool: suspend until a named external signal is
/// delivered (via `truss signal …`). Intercepted by the loop.
pub fn await_signal_tool_def() -> ToolDef {
    ToolDef {
        name: "await_signal".into(),
        description: "Suspend the run until an external signal with the given name is \
                      delivered (e.g. a human approval or a finished job). The process \
                      may exit entirely and resume later with full context."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The signal name to wait for" },
                "reason": { "type": "string", "description": "Why the run is waiting" }
            },
            "required": ["name"]
        }),
    }
}

/// Start a new durable run, or advance/resume an existing one.
///
/// Stage A: the journal is authoritative. We reconstruct the conversation from
/// the journal (replaying recorded model/tool outcomes), satisfy any pending
/// suspend, then run the live loop from the frontier. The loop's next action is
/// a pure function of the last reconstructed message, so every crash point
/// recovers correctly and recorded steps are never re-executed.
pub async fn run(
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    ctx: &Ctx,
    store: &Store,
    session_id: &str,
    cfg: &DurableConfig,
    prompt: Option<String>,
) -> Result<DurableOutcome, HarnessError> {
    let model = cfg
        .model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());

    let entries = store.entries(session_id).map_err(other)?;
    let fresh_model_run = !entries.iter().any(|entry| {
        matches!(
            entry.kind.as_str(),
            "model_result" | "tool_result" | "run_completed" | "run_failed"
        )
    });
    let mut messages: Vec<Message>;
    let mut message_sources: Vec<Vec<LedgerRef>>;

    if entries.is_empty() {
        // Fresh run.
        let prompt = prompt
            .ok_or_else(|| HarnessError::Other("new durable run requires a prompt".into()))?;
        store
            .create_session(
                session_id,
                Some(&model),
                &ctx.principal.tenant,
                &ctx.principal.user,
            )
            .map_err(other)?;
        let started = store
            .append(
                session_id,
                "run_started",
                &json!({ "prompt": prompt, "model": model, "system": cfg.system }),
            )
            .map_err(other)?;
        messages = vec![Message::user(prompt)];
        message_sources = vec![vec![ledger_ref(session_id, started.seq)]];
    } else {
        // Resume / recover: rebuild state from the journal.
        let (rec, sources) = reconstruct_with_sources(session_id, &entries);
        if rec.terminal {
            return Ok(DurableOutcome::Completed {
                final_text: rec.final_text,
            });
        }
        messages = rec.messages;
        message_sources = sources;

        if let Some(susp) = rec.pending_suspend {
            match susp.await_kind.as_str() {
                "signal" => {
                    if susp.approval_gate && !approval_gate_has_signed_approval(&entries) {
                        return Ok(DurableOutcome::StillWaiting {
                            await_desc: format!("signal '{}'", susp.signal_name),
                        });
                    }
                    match store
                        .take_pending_signal(session_id, &susp.signal_name)
                        .map_err(other)?
                    {
                        Some(payload) => {
                            if let Some(kind) = susp.resume_payload["kind"].as_str() {
                                if kind == "mcp_pending" {
                                    crate::mcp::record_pending_resume(
                                        store,
                                        session_id,
                                        &susp.resume_payload,
                                        &payload,
                                        susp.resume_is_error,
                                    )
                                    .map_err(HarnessError::Tool)?;
                                }
                            }
                            let received = store
                                .append(
                                    session_id,
                                    "signal_received",
                                    &json!({ "name": susp.signal_name, "payload": payload }),
                                )
                                .map_err(other)?;
                            if !susp.approval_gate {
                                let pending_tool_id = susp.pending_tool_id.clone();
                                let (content, is_error) = crate::replay::signal_tool_result(
                                    &susp,
                                    &susp.signal_name,
                                    &payload,
                                );
                                push_tool_block_with_source(
                                    &mut messages,
                                    &mut message_sources,
                                    Block::ToolResult {
                                        id: pending_tool_id,
                                        content,
                                        is_error,
                                    },
                                    ledger_ref(session_id, received.seq),
                                );
                            }
                            store.set_status(session_id, "running").map_err(other)?;
                        }
                        None => {
                            return Ok(DurableOutcome::StillWaiting {
                                await_desc: format!("signal '{}'", susp.signal_name),
                            })
                        }
                    }
                }
                _ => {
                    if now_ms() < susp.until_ms {
                        return Ok(DurableOutcome::StillWaiting {
                            await_desc: format!("timer until epoch-ms {}", susp.until_ms),
                        });
                    }
                    let fired = store
                        .append(session_id, "timer_fired", &json!({ "reason": susp.reason }))
                        .map_err(other)?;
                    push_tool_block_with_source(
                        &mut messages,
                        &mut message_sources,
                        Block::ToolResult {
                            id: susp.pending_tool_id,
                            content: crate::replay::timer_resume_note(),
                            is_error: false,
                        },
                        ledger_ref(session_id, fired.seq),
                    );
                    store.set_status(session_id, "running").map_err(other)?;
                }
            }
        } else {
            // Mid-run crash recovery: continue from the frontier.
            store.set_status(session_id, "running").map_err(other)?;
        }
    }

    let mut growth_estimator =
        ContextGrowthEstimator::from_projected_tokens(entries.iter().filter_map(|entry| {
            (entry.kind == "model_result")
                .then(|| entry.payload["usage"]["input"].as_u64())
                .flatten()
        }));

    // ---- live loop: the next action is a function of the last message --------
    let mut model_calls = 0usize;
    loop {
        ctx.check()?;
        ctx.budget.check()?;

        let last = messages.last();
        let assistant_last = last.map(|m| m.role == Role::Assistant).unwrap_or(false);
        let has_tool_use = last
            .map(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. })))
            .unwrap_or(false);

        if assistant_last && has_tool_use {
            let content = messages.last().unwrap().content.clone();

            // Suspend if the model called `wait` (timer) or `await_signal`.
            if let Some((id, seconds, reason)) = find_wait(&content) {
                let until_ms = now_ms() + seconds * 1000;
                store
                    .append(
                        session_id,
                        "suspended",
                        &json!({ "await_kind": "timer", "until_ms": until_ms, "pending_tool_id": id, "reason": reason }),
                    )
                    .map_err(other)?;
                store.set_status(session_id, "suspended").map_err(other)?;
                return Ok(DurableOutcome::Suspended {
                    await_desc: format!("timer until epoch-ms {until_ms}"),
                    reason,
                });
            }
            if let Some((id, name, reason)) = find_await_signal(&content) {
                store
                    .append(
                        session_id,
                        "suspended",
                        &json!({ "await_kind": "signal", "signal_name": name, "pending_tool_id": id, "reason": reason }),
                    )
                    .map_err(other)?;
                store.set_status(session_id, "suspended").map_err(other)?;
                return Ok(DurableOutcome::Suspended {
                    await_desc: format!("signal '{name}'"),
                    reason,
                });
            }

            // Execute the requested tools (live frontier work).
            let produced_by = store
                .last_entry(session_id)
                .map_err(other)?
                .filter(|entry| entry.kind == "model_result")
                .map(|entry| LedgerRef {
                    session: session_id.to_string(),
                    seq: entry.seq,
                });
            let mut results = Vec::new();
            let mut result_sources = Vec::new();
            for block in &content {
                if let Block::ToolUse { id, name, input } = block {
                    if let Some(trigger) = cfg.approval_gates.should_gate_tool(name, id) {
                        let trigger_key = approval_trigger_key(&trigger);
                        if !approval_gate_released(
                            &store.entries(session_id).map_err(other)?,
                            &trigger_key,
                        ) {
                            suspend_for_approval_gate(
                                store,
                                session_id,
                                trigger,
                                format!("approval required before tool '{name}'"),
                            )?;
                            return Ok(DurableOutcome::Suspended {
                                await_desc: format!("signal '{APPROVAL_GATE_SIGNAL}'"),
                                reason: format!("approval required before tool '{name}'"),
                            });
                        }
                    }
                    let tool_result = if name == "memory" && !cfg.memory_tool {
                        Err(crate::error::ToolError::PermissionDenied(
                            "memory tool disabled for this session".into(),
                        ))
                    } else {
                        tools
                            .call_with_ledger(ctx, name, input.clone(), produced_by.clone())
                            .await
                    };
                    let (out, is_error) = match tool_result {
                        Ok(o) => (o, false),
                        Err(crate::error::ToolError::Suspend {
                            signal_name,
                            reason,
                            resume_payload,
                        }) => {
                            if !results.is_empty() {
                                messages.push(Message {
                                    role: Role::Tool,
                                    content: results,
                                });
                                message_sources.push(result_sources);
                            }
                            store
                                .append(
                                    session_id,
                                    "suspended",
                                    &json!({
                                        "await_kind": "signal",
                                        "signal_name": signal_name,
                                        "pending_tool_id": id,
                                        "reason": reason,
                                        "resume_tool_result_from_signal": true,
                                        "resume_is_error": false,
                                        "resume_payload": resume_payload,
                                    }),
                                )
                                .map_err(other)?;
                            store.set_status(session_id, "suspended").map_err(other)?;
                            return Ok(DurableOutcome::Suspended {
                                await_desc: format!("signal '{signal_name}'"),
                                reason,
                            });
                        }
                        Err(e) => (format!("error: {e}"), true),
                    };
                    let result_entry = store
                        .append(
                            session_id,
                            "tool_result",
                            &json!({ "call_id": id, "name": name, "input": input, "content": out, "is_error": is_error }),
                        )
                        .map_err(other)?;
                    result_sources.push(ledger_ref(session_id, result_entry.seq));
                    results.push(Block::ToolResult {
                        id: id.clone(),
                        content: out,
                        is_error,
                    });
                }
            }
            messages.push(Message {
                role: Role::Tool,
                content: results,
            });
            message_sources.push(result_sources);
            continue;
        }

        if assistant_last {
            // No tool use → terminal.
            let final_text = messages.last().unwrap().text();
            store
                .append(
                    session_id,
                    "run_completed",
                    &json!({ "final_text": final_text }),
                )
                .map_err(other)?;
            store.set_status(session_id, "completed").map_err(other)?;
            return Ok(DurableOutcome::Completed { final_text });
        }

        // Last is user / tool → call the model.
        model_calls += 1;
        if model_calls > cfg.max_steps {
            store
                .append(session_id, "run_failed", &json!({ "error": "max steps" }))
                .map_err(other)?;
            store.set_status(session_id, "failed").map_err(other)?;
            return Err(HarnessError::MaxSteps(cfg.max_steps));
        }

        let mut tool_defs = tools.defs_for(ctx);
        if !cfg.memory_tool {
            tool_defs.retain(|tool| tool.name != "memory");
        }
        tool_defs.push(wait_tool_def());
        tool_defs.push(await_signal_tool_def());
        let projected_history = if cfg.max_context_msgs > 0 {
            crate::projection::project(&messages, cfg.max_context_msgs)
        } else {
            messages.clone()
        };
        let (lesson_recall, playbook_recall) = if fresh_model_run && model_calls == 1 {
            cfg.memory_store
                .as_ref()
                .map(|memory_store| {
                    let task_context = recall_task_context(cfg, &projected_history);
                    let lessons = crate::memory::recall_verified_lessons(
                        memory_store.as_ref(),
                        &task_context,
                        &cfg.memory,
                    )
                    .map_err(|e| HarnessError::Other(e.to_string()))?;
                    let playbooks = crate::memory::recall_verified_playbooks(
                        memory_store.as_ref(),
                        &task_context,
                        &cfg.memory,
                    )
                    .map_err(|e| HarnessError::Other(e.to_string()))?;
                    Ok::<_, HarnessError>((lessons, playbooks))
                })
                .transpose()?
                .unwrap_or((None, None))
        } else {
            (None, None)
        };
        let mut assembly = crate::memory::ContextAssembler::new(
            cfg.persona.clone(),
            cfg.principal.clone(),
            cfg.system.clone(),
        )
        .goal_ledger(cfg.goal_ledger.clone())
        .step(model_calls)
        .memory_delta(None, cfg.memory_cursor)
        .verified_lesson_recall(lesson_recall)
        .verified_playbook_recall(playbook_recall)
        .scratch(None)
        .tools(&tool_defs)
        .history(projected_history);
        if cfg.goal_ledger.is_none() && cfg.persona.is_none() && cfg.principal.is_none() {
            assembly = assembly.use_legacy_system_when_empty(true);
        }
        let assembled = assembly.assemble();
        let req = CompletionRequest {
            system: assembled.legacy_system,
            system_blocks: assembled.system,
            messages: assembled.messages,
            tools: tool_defs,
            max_tokens: cfg.max_tokens,
            model: Some(model.clone()),
            temperature: cfg.temperature,
            reasoning: cfg.reasoning,
        };

        let resp = crate::retry::with_retry(ctx, &cfg.retry, |_| provider.complete(ctx, &req))
            .await
            .map_err(|e| match e {
                crate::error::ProviderError::Cancelled => HarnessError::Cancelled,
                other => HarnessError::Provider(other),
            })?;

        let cost = crate::pricing::cost_usd(
            &resp.model,
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );
        let budget_before = ctx.budget.snapshot();
        ctx.budget
            .record_usage(resp.usage.input_tokens, resp.usage.output_tokens, cost);
        ctx.budget.record_step();
        let budget_after = ctx.budget.snapshot();
        let pressure = ContextGauge::new(crate::memory::context_pressure_window(
            provider.as_ref(),
            Some(&resp.model),
            Some(&cfg.memory),
        ))
        .measure(resp.usage.input_tokens);
        growth_estimator.observe(pressure.tokens);
        ctx.emit(AgentEvent::ContextPressure {
            step: model_calls as u64,
            tokens: pressure.tokens,
            window: pressure.window,
            pressure: pressure.pressure,
        });
        let compact_ahead = maybe_emit_compaction_imminent(
            ctx,
            &cfg.memory.compaction,
            model_calls as u64,
            &pressure,
            &growth_estimator,
        );

        let content_json =
            serde_json::to_value(&resp.content).map_err(|e| HarnessError::Other(e.to_string()))?;
        let model_entry = store
            .append(
                session_id,
                "model_result",
                &json!({
                    "content": content_json,
                    "stop_reason": format!("{:?}", resp.stop_reason),
                    "usage": { "input": resp.usage.input_tokens, "output": resp.usage.output_tokens },
                }),
            )
            .map_err(other)?;
        if let Some(trigger) =
            cfg.approval_gates
                .should_gate_spend(&budget_before, &budget_after, cost)
        {
            suspend_for_approval_gate(
                store,
                session_id,
                trigger,
                "approval required for spend threshold".to_string(),
            )?;
            return Ok(DurableOutcome::Suspended {
                await_desc: format!("signal '{APPROVAL_GATE_SIGNAL}'"),
                reason: "approval required for spend threshold".into(),
            });
        }
        messages.push(Message {
            role: Role::Assistant,
            content: resp.content,
        });
        message_sources.push(vec![ledger_ref(session_id, model_entry.seq)]);
        maybe_compact_history(
            BoundaryCompaction {
                ctx,
                store,
                session_id,
                cfg,
                step: model_calls as u64,
                before_tokens: pressure.tokens,
                pressure: pressure.pressure,
                compact_ahead,
            },
            &mut messages,
            &mut message_sources,
        )?;
    }
}

fn find_wait(content: &[Block]) -> Option<(String, i64, String)> {
    content.iter().find_map(|b| match b {
        Block::ToolUse { id, name, input } if name == "wait" => Some((
            id.clone(),
            input["seconds"].as_i64().unwrap_or(0),
            input["reason"].as_str().unwrap_or("").to_string(),
        )),
        _ => None,
    })
}

fn suspend_for_approval_gate(
    store: &Store,
    session_id: &str,
    trigger: ApprovalGateTrigger,
    reason: String,
) -> Result<(), HarnessError> {
    let trigger_key = approval_trigger_key(&trigger);
    store
        .append(
            session_id,
            "suspended",
            &json!({
                "await_kind": "signal",
                "signal_name": APPROVAL_GATE_SIGNAL,
                "reason": reason,
                "approval_gate": {
                    "trigger_key": trigger_key,
                    "trigger": trigger,
                    "decision_kind": "human_review",
                },
            }),
        )
        .map_err(other)?;
    store.set_status(session_id, "suspended").map_err(other)?;
    Ok(())
}

fn approval_trigger_key(trigger: &ApprovalGateTrigger) -> String {
    match trigger {
        ApprovalGateTrigger::Tool {
            tool_name,
            tool_call_id,
            ..
        } => {
            format!("tool:{tool_name}:{tool_call_id}")
        }
        ApprovalGateTrigger::Spend { .. } => "spend".to_string(),
    }
}

fn approval_gate_released(entries: &[crate::store::JournalEntry], trigger_key: &str) -> bool {
    let mut pending_match = false;
    let mut released = false;
    for entry in entries {
        match entry.kind.as_str() {
            "suspended" if crate::store::is_approval_gate_payload(&entry.payload) => {
                pending_match = entry
                    .payload
                    .get("approval_gate")
                    .and_then(|gate| gate.get("trigger_key"))
                    .and_then(|value| value.as_str())
                    == Some(trigger_key);
                released = false;
            }
            "signal_received"
                if pending_match
                    && entry.payload.get("name").and_then(|v| v.as_str())
                        == Some(APPROVAL_GATE_SIGNAL) =>
            {
                released = true;
                pending_match = false;
            }
            _ => {}
        }
    }
    released
}

fn approval_gate_has_signed_approval(entries: &[crate::store::JournalEntry]) -> bool {
    let mut after_pending_gate = false;
    let mut approved = false;
    for entry in entries {
        match entry.kind.as_str() {
            "suspended" if crate::store::is_approval_gate_payload(&entry.payload) => {
                after_pending_gate = true;
                approved = false;
            }
            crate::store::HUMAN_REVIEW_KIND if after_pending_gate => {
                approved =
                    entry.payload.get("decision").and_then(|v| v.as_str()) == Some("approved");
            }
            "signal_received"
                if entry.payload.get("name").and_then(|v| v.as_str())
                    == Some(APPROVAL_GATE_SIGNAL) =>
            {
                after_pending_gate = false;
            }
            _ => {}
        }
    }
    approved
}

struct BoundaryCompaction<'a> {
    ctx: &'a Ctx,
    store: &'a Store,
    session_id: &'a str,
    cfg: &'a DurableConfig,
    step: u64,
    before_tokens: u64,
    pressure: f64,
    compact_ahead: bool,
}

fn maybe_emit_compaction_imminent(
    ctx: &Ctx,
    policy: &crate::memory::CompactionPolicy,
    step: u64,
    pressure: &ContextPressure,
    growth_estimator: &ContextGrowthEstimator,
) -> bool {
    if !policy.enabled() || !policy.anticipate {
        return false;
    }
    let Some(estimate) =
        growth_estimator.estimate(pressure.tokens, pressure.window, policy.threshold)
    else {
        return false;
    };
    if estimate.steps_to_threshold <= policy.lookahead_steps {
        ctx.emit(AgentEvent::CompactionImminent {
            step,
            steps_to_threshold: estimate.steps_to_threshold,
            pressure: pressure.pressure,
            window: pressure.window,
        });
        return pressure.pressure < policy.threshold;
    }
    false
}

fn maybe_compact_history(
    boundary: BoundaryCompaction<'_>,
    messages: &mut Vec<Message>,
    message_sources: &mut Vec<Vec<LedgerRef>>,
) -> Result<(), HarnessError> {
    let policy = &boundary.cfg.memory.compaction;
    if !policy.enabled() || (boundary.pressure < policy.threshold && !boundary.compact_ahead) {
        return Ok(());
    }
    let Some(memory_store) = boundary.cfg.memory_store.as_deref() else {
        return Ok(());
    };
    let Some(summary) = crate::projection::project_with_summary(messages, policy.min_tail) else {
        return Ok(());
    };

    let elided_refs = message_sources[summary.elided_start..summary.elided_end]
        .iter()
        .flat_map(|refs| refs.iter().cloned())
        .collect::<Vec<_>>();
    if elided_refs.is_empty() {
        return Ok(());
    }
    let start = elided_refs
        .iter()
        .min_by_key(|ledger| ledger.seq)
        .cloned()
        .expect("checked non-empty");
    let end = elided_refs
        .iter()
        .max_by_key(|ledger| ledger.seq)
        .cloned()
        .expect("checked non-empty");
    let elided_range = LedgerRange { start, end };

    let produced_by = boundary
        .store
        .last_entry(boundary.session_id)
        .map_err(other)?
        .map(|entry| ledger_ref(boundary.session_id, entry.seq))
        .unwrap_or_else(|| elided_range.end.clone());
    let summary_body = format!(
        "[context summary]\nmode: {}\nelided_journal_range: {}#{}..{}#{}\n\n{}",
        compaction_mode_name(policy.mode),
        elided_range.start.session,
        elided_range.start.seq,
        elided_range.end.session,
        elided_range.end.seq,
        summary.summary.text()
    );
    let summary_id = memory_store
        .write(MemoryDraft {
            kind: EntryKind::ContextSummary,
            key: Some(format!(
                "{}:{}-{}",
                boundary.session_id, elided_range.start.seq, elided_range.end.seq
            )),
            body: summary_body,
            produced_by,
            sources: elided_refs,
        })
        .map_err(|e| HarnessError::Other(e.to_string()))?;

    let projected_messages = match policy.mode {
        CompactionMode::Off => messages.clone(),
        CompactionMode::Summarize => summary.projected,
        CompactionMode::Evict => {
            let mut projected = Vec::with_capacity(summary.projected.len().saturating_sub(1));
            projected.push(messages[0].clone());
            projected.extend_from_slice(&messages[summary.elided_end..]);
            projected
        }
    };
    let after_tokens = estimate_after_tokens(boundary.before_tokens, messages, &projected_messages);

    let summary_id_text = summary_id.0.clone();
    let payload = json!({
        "mode": compaction_mode_name(policy.mode),
        "threshold": policy.threshold,
        "min_tail": policy.min_tail,
        "anticipate": policy.anticipate,
        "lookahead_steps": policy.lookahead_steps,
        "exempt_tiers": policy.exempt_tiers.clone(),
        "before_tokens": boundary.before_tokens,
        "after_tokens": after_tokens,
        "after_tokens_estimated": true,
        "summary_memory_id": summary_id_text,
        "summary_trust": "unverified",
        "verification_policy": boundary.cfg.memory.verification,
        "elided_range": elided_range.clone(),
    });
    let compaction_entry = boundary
        .store
        .append(boundary.session_id, "compaction_performed", &payload)
        .map_err(other)?;
    let compaction_ref = ledger_ref(boundary.session_id, compaction_entry.seq);

    *messages = projected_messages;
    *message_sources = compacted_sources(
        message_sources,
        summary.elided_end,
        policy.min_tail,
        policy.mode,
        compaction_ref.clone(),
    );
    boundary.ctx.emit(AgentEvent::CompactionPerformed {
        step: boundary.step,
        mode: compaction_mode_name(policy.mode).to_string(),
        before_tokens: boundary.before_tokens,
        after_tokens,
        summary_memory_id: summary_id.0,
        elided_range,
    });
    Ok(())
}

fn compacted_sources(
    sources: &[Vec<LedgerRef>],
    elided_end: usize,
    min_tail: usize,
    mode: CompactionMode,
    summary_ref: LedgerRef,
) -> Vec<Vec<LedgerRef>> {
    let tail_start = sources.len().saturating_sub(min_tail);
    let mut compacted = Vec::with_capacity(sources.len().saturating_sub(elided_end) + 2);
    compacted.push(sources[0].clone());
    if matches!(mode, CompactionMode::Summarize) {
        compacted.push(vec![summary_ref]);
    }
    compacted.extend_from_slice(&sources[tail_start..]);
    compacted
}

fn estimate_after_tokens(before_tokens: u64, before: &[Message], after: &[Message]) -> u64 {
    let before_weight = projection_weight(before);
    if before_tokens == 0 || before_weight == 0 {
        return 0;
    }
    let after_weight = projection_weight(after).max(1);
    ((before_tokens as u128 * after_weight as u128).div_ceil(before_weight as u128)) as u64
}

fn projection_weight(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|message| 1 + message.content.len() as u64)
        .sum()
}

fn compaction_mode_name(mode: CompactionMode) -> &'static str {
    match mode {
        CompactionMode::Off => "off",
        CompactionMode::Summarize => "summarize",
        CompactionMode::Evict => "evict",
    }
}

fn reconstruct_with_sources(
    session_id: &str,
    entries: &[crate::store::JournalEntry],
) -> (crate::replay::Reconstructed, Vec<Vec<LedgerRef>>) {
    let mut r = crate::replay::Reconstructed::default();
    let mut sources: Vec<Vec<LedgerRef>> = Vec::new();
    let mut last_suspend: Option<crate::replay::SuspendInfo> = None;

    for entry in entries {
        match entry.kind.as_str() {
            "run_started" => {
                if let Some(prompt) = entry.payload["prompt"].as_str() {
                    r.messages.push(Message::user(prompt));
                    sources.push(vec![ledger_ref(session_id, entry.seq)]);
                }
            }
            "model_result" => {
                let content: Vec<Block> =
                    serde_json::from_value(entry.payload["content"].clone()).unwrap_or_default();
                r.messages.push(Message {
                    role: Role::Assistant,
                    content,
                });
                sources.push(vec![ledger_ref(session_id, entry.seq)]);
            }
            "tool_result" => {
                let block = Block::ToolResult {
                    id: entry.payload["call_id"].as_str().unwrap_or("").to_string(),
                    content: entry.payload["content"].as_str().unwrap_or("").to_string(),
                    is_error: entry.payload["is_error"].as_bool().unwrap_or(false),
                };
                push_tool_block_with_source(
                    &mut r.messages,
                    &mut sources,
                    block,
                    ledger_ref(session_id, entry.seq),
                );
            }
            "suspended" => {
                let info = crate::replay::SuspendInfo {
                    await_kind: entry.payload["await_kind"]
                        .as_str()
                        .unwrap_or("timer")
                        .to_string(),
                    until_ms: entry.payload["until_ms"].as_i64().unwrap_or(0),
                    signal_name: entry.payload["signal_name"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    pending_tool_id: entry.payload["pending_tool_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    reason: entry.payload["reason"].as_str().unwrap_or("").to_string(),
                    approval_gate: crate::store::is_approval_gate_payload(&entry.payload),
                    resume_tool_result_from_signal: entry.payload["resume_tool_result_from_signal"]
                        .as_bool()
                        .unwrap_or(false),
                    resume_is_error: entry.payload["resume_is_error"].as_bool().unwrap_or(false),
                    resume_payload: entry.payload["resume_payload"].clone(),
                };
                last_suspend = Some(info.clone());
                r.pending_suspend = Some(info);
            }
            "timer_fired" => {
                let id = last_suspend
                    .as_ref()
                    .map(|s| s.pending_tool_id.clone())
                    .unwrap_or_default();
                push_tool_block_with_source(
                    &mut r.messages,
                    &mut sources,
                    Block::ToolResult {
                        id,
                        content: crate::replay::timer_resume_note(),
                        is_error: false,
                    },
                    ledger_ref(session_id, entry.seq),
                );
                r.pending_suspend = None;
            }
            "signal_received" => {
                if last_suspend
                    .as_ref()
                    .map(|s| s.approval_gate)
                    .unwrap_or(false)
                {
                    r.pending_suspend = None;
                    continue;
                }
                let id = last_suspend
                    .as_ref()
                    .map(|s| s.pending_tool_id.clone())
                    .unwrap_or_default();
                let name = entry.payload["name"].as_str().unwrap_or("");
                let payload = entry.payload["payload"].as_str().unwrap_or("");
                let (content, is_error) = last_suspend
                    .as_ref()
                    .map(|info| crate::replay::signal_tool_result(info, name, payload))
                    .unwrap_or_else(|| (crate::replay::signal_resume_note(name, payload), false));
                push_tool_block_with_source(
                    &mut r.messages,
                    &mut sources,
                    Block::ToolResult {
                        id,
                        content,
                        is_error,
                    },
                    ledger_ref(session_id, entry.seq),
                );
                r.pending_suspend = None;
            }
            "run_completed" => {
                r.terminal = true;
                r.final_text = entry.payload["final_text"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
            }
            "run_failed" => r.failed = true,
            _ => {}
        }
    }
    (r, sources)
}

fn push_tool_block_with_source(
    messages: &mut Vec<Message>,
    sources: &mut Vec<Vec<LedgerRef>>,
    block: Block,
    source: LedgerRef,
) {
    if let Some(last) = messages.last_mut() {
        if last.role == Role::Tool {
            last.content.push(block);
            if let Some(last_sources) = sources.last_mut() {
                last_sources.push(source);
            }
            return;
        }
    }
    messages.push(Message {
        role: Role::Tool,
        content: vec![block],
    });
    sources.push(vec![source]);
}

fn ledger_ref(session_id: &str, seq: i64) -> LedgerRef {
    LedgerRef {
        session: session_id.to_string(),
        seq,
    }
}

fn recall_task_context(cfg: &DurableConfig, messages: &[Message]) -> String {
    let mut task_context = String::new();
    if let Some(goal) = &cfg.goal_ledger {
        task_context.push_str(&goal.intent);
        for clause in &goal.definition_of_done {
            task_context.push('\n');
            task_context.push_str(clause);
        }
    }
    if let Some(first_user) = messages.iter().find(|message| message.role == Role::User) {
        if !task_context.is_empty() {
            task_context.push('\n');
        }
        task_context.push_str(&first_user.text());
    }
    task_context
}

fn find_await_signal(content: &[Block]) -> Option<(String, String, String)> {
    content.iter().find_map(|b| match b {
        Block::ToolUse { id, name, input } if name == "await_signal" => Some((
            id.clone(),
            input["name"].as_str().unwrap_or("").to_string(),
            input["reason"].as_str().unwrap_or("").to_string(),
        )),
        _ => None,
    })
}

fn other(e: anyhow::Error) -> HarnessError {
    HarnessError::Other(e.to_string())
}
