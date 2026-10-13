//! Journal replay (Stage A).
//!
//! The journal is the source of truth. [`reconstruct`] folds the recorded entries
//! back into the conversation `messages` — turning each recorded `model_result`
//! and `tool_result` into the assistant/tool turns that produced them. This is
//! what makes resume *exact*: recorded activities are **replayed** (re-derived
//! from the log), never re-executed. The live loop then continues from the
//! frontier (the first un-recorded step).
//!
//! Snapshots are now just optional checkpoints; correctness flows from here.

use crate::message::{Block, Message, Role};
use crate::store::JournalEntry;

/// The await recorded by a `suspended` entry that hasn't been satisfied yet.
#[derive(Debug, Clone)]
pub struct SuspendInfo {
    pub await_kind: String, // "timer" | "signal"
    pub until_ms: i64,
    pub signal_name: String,
    pub pending_tool_id: String,
    pub reason: String,
    pub approval_gate: bool,
    pub resume_tool_result_from_signal: bool,
    pub resume_is_error: bool,
    pub resume_payload: serde_json::Value,
}

/// The reconstructed state of a run, derived purely from its journal.
#[derive(Debug, Default)]
pub struct Reconstructed {
    pub messages: Vec<Message>,
    pub terminal: bool,
    pub final_text: String,
    pub failed: bool,
    /// Set iff the run is currently suspended (a `suspended` entry with no
    /// following `timer_fired` / `signal_received`).
    pub pending_suspend: Option<SuspendInfo>,
}

/// Append a tool-result block, grouping consecutive results into one Tool message
/// (mirrors how the live loop emits them).
pub fn push_tool_block(messages: &mut Vec<Message>, block: Block) {
    if let Some(last) = messages.last_mut() {
        if last.role == Role::Tool {
            last.content.push(block);
            return;
        }
    }
    messages.push(Message {
        role: Role::Tool,
        content: vec![block],
    });
}

/// The deterministic note injected when a wait/signal is satisfied. Must contain
/// no wall-clock/random values so replay is faithful.
pub fn timer_resume_note() -> String {
    "Resumed: the wait elapsed.".to_string()
}

pub fn signal_resume_note(name: &str, payload: &str) -> String {
    if payload.is_empty() {
        format!("Signal '{name}' received.")
    } else {
        format!("Signal '{name}' received: {payload}")
    }
}

pub fn signal_tool_result(info: &SuspendInfo, name: &str, payload: &str) -> (String, bool) {
    if info.resume_tool_result_from_signal {
        (payload.to_string(), info.resume_is_error)
    } else {
        (signal_resume_note(name, payload), false)
    }
}

/// Fold a journal into conversation state.
pub fn reconstruct(entries: &[JournalEntry]) -> Reconstructed {
    let mut r = Reconstructed::default();
    let mut last_suspend: Option<SuspendInfo> = None;

    for e in entries {
        match e.kind.as_str() {
            "run_started" => {
                if let Some(p) = e.payload["prompt"].as_str() {
                    r.messages.push(Message::user(p));
                }
            }
            "model_result" => {
                let content: Vec<Block> =
                    serde_json::from_value(e.payload["content"].clone()).unwrap_or_default();
                r.messages.push(Message {
                    role: Role::Assistant,
                    content,
                });
            }
            "tool_result" => {
                let block = Block::ToolResult {
                    id: e.payload["call_id"].as_str().unwrap_or("").to_string(),
                    content: e.payload["content"].as_str().unwrap_or("").to_string(),
                    is_error: e.payload["is_error"].as_bool().unwrap_or(false),
                };
                push_tool_block(&mut r.messages, block);
            }
            "suspended" => {
                let info = SuspendInfo {
                    await_kind: e.payload["await_kind"]
                        .as_str()
                        .unwrap_or("timer")
                        .to_string(),
                    until_ms: e.payload["until_ms"].as_i64().unwrap_or(0),
                    signal_name: e.payload["signal_name"].as_str().unwrap_or("").to_string(),
                    pending_tool_id: e.payload["pending_tool_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    reason: e.payload["reason"].as_str().unwrap_or("").to_string(),
                    approval_gate: crate::store::is_approval_gate_payload(&e.payload),
                    resume_tool_result_from_signal: e.payload["resume_tool_result_from_signal"]
                        .as_bool()
                        .unwrap_or(false),
                    resume_is_error: e.payload["resume_is_error"].as_bool().unwrap_or(false),
                    resume_payload: e.payload["resume_payload"].clone(),
                };
                last_suspend = Some(info.clone());
                r.pending_suspend = Some(info);
            }
            "timer_fired" => {
                let id = last_suspend
                    .as_ref()
                    .map(|s| s.pending_tool_id.clone())
                    .unwrap_or_default();
                push_tool_block(
                    &mut r.messages,
                    Block::ToolResult {
                        id,
                        content: timer_resume_note(),
                        is_error: false,
                    },
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
                let name = e.payload["name"].as_str().unwrap_or("");
                let payload = e.payload["payload"].as_str().unwrap_or("");
                let (content, is_error) = last_suspend
                    .as_ref()
                    .map(|info| signal_tool_result(info, name, payload))
                    .unwrap_or_else(|| (signal_resume_note(name, payload), false));
                push_tool_block(
                    &mut r.messages,
                    Block::ToolResult {
                        id,
                        content,
                        is_error,
                    },
                );
                r.pending_suspend = None;
            }
            "run_completed" => {
                r.terminal = true;
                r.final_text = e.payload["final_text"].as_str().unwrap_or("").to_string();
            }
            "run_failed" => r.failed = true,
            _ => {}
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use serde_json::json;

    #[test]
    fn reconstructs_messages_and_detects_suspend() {
        // Build a small journal via the store (which hash-chains it for us).
        let store = Store::open_memory().unwrap();
        store
            .create_session("s", Some("m"), "local", "local")
            .unwrap();
        store
            .append("s", "run_started", &json!({ "prompt": "hi" }))
            .unwrap();
        let tu = vec![Block::ToolUse {
            id: "c1".into(),
            name: "ping".into(),
            input: json!({}),
        }];
        store
            .append(
                "s",
                "model_result",
                &json!({ "content": serde_json::to_value(&tu).unwrap() }),
            )
            .unwrap();
        store
            .append(
                "s",
                "tool_result",
                &json!({ "call_id": "c1", "content": "pong", "is_error": false }),
            )
            .unwrap();
        store
            .append("s", "suspended", &json!({ "await_kind": "signal", "signal_name": "go", "pending_tool_id": "c2", "reason": "wait" }))
            .unwrap();

        let rec = reconstruct(&store.entries("s").unwrap());
        // user, assistant(tool_use), tool(result)
        assert_eq!(rec.messages.len(), 3);
        assert_eq!(rec.messages[0].text(), "hi");
        assert!(matches!(rec.messages[1].role, Role::Assistant));
        assert!(matches!(rec.messages[2].role, Role::Tool));
        assert!(!rec.terminal);
        let susp = rec.pending_suspend.expect("suspended");
        assert_eq!(susp.await_kind, "signal");
        assert_eq!(susp.signal_name, "go");
    }
}
