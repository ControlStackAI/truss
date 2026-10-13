//! Context projection (M4).
//!
//! Durable ≠ infinite context. The journal/message history grows without bound,
//! but the model's context window does not. What we feed the model each step is a
//! **derived projection** of the full history — recomputed, never the source of
//! truth (per `docs/durable-execution.md` §10):
//!
//! * **Head** — the first user message (the task), always kept.
//! * **Tail** — the most recent `keep_tail` turns, verbatim.
//! * **Middle** — replaced by a single compact summary message.
//!
//! This M4 implementation summarizes the middle **structurally** (a deterministic
//! digest: how many turns were elided and which tools they used). Replacing that
//! with a model-written summary is a drop-in refinement — and, because the
//! summary is produced by a journaled activity, it stays replay-faithful.

use crate::message::{Block, Message, Role};

#[derive(Debug, Clone)]
pub struct ProjectionSummary {
    pub projected: Vec<Message>,
    pub summary: Message,
    pub elided_start: usize,
    pub elided_end: usize,
}

/// Project `messages` down to Head + summary + Tail when it exceeds the budget.
/// Returns the input unchanged when it already fits.
///
/// `keep_tail` is the number of most-recent messages to keep verbatim.
pub fn project(messages: &[Message], keep_tail: usize) -> Vec<Message> {
    project_with_summary(messages, keep_tail)
        .map(|summary| summary.projected)
        .unwrap_or_else(|| messages.to_vec())
}

/// Project with metadata about the elided middle. The elided end is exclusive.
pub fn project_with_summary(messages: &[Message], keep_tail: usize) -> Option<ProjectionSummary> {
    // Head (1) + summary (1) + tail (keep_tail). Below that, no point compacting.
    let threshold = keep_tail + 2;
    if messages.len() <= threshold {
        return None;
    }

    let head = &messages[0];
    let tail_start = messages.len() - keep_tail;
    let middle = &messages[1..tail_start];
    let summary = summarize(middle);

    let mut out = Vec::with_capacity(keep_tail + 2);
    out.push(head.clone());
    out.push(summary.clone());
    out.extend_from_slice(&messages[tail_start..]);
    Some(ProjectionSummary {
        projected: out,
        summary,
        elided_start: 1,
        elided_end: tail_start,
    })
}

/// Deterministic structural summary of the elided middle turns.
fn summarize(middle: &[Message]) -> Message {
    let mut assistant = 0usize;
    let mut user = 0usize;
    let mut tool = 0usize;
    let mut tool_names: Vec<String> = Vec::new();

    for m in middle {
        match m.role {
            Role::Assistant => assistant += 1,
            Role::User => user += 1,
            Role::Tool => tool += 1,
        }
        for b in &m.content {
            if let Block::ToolUse { name, .. } = b {
                if !tool_names.contains(name) {
                    tool_names.push(name.clone());
                }
            }
        }
    }

    let tools = if tool_names.is_empty() {
        "none".to_string()
    } else {
        tool_names.join(", ")
    };
    let text = format!(
        "[context summary: {} earlier turns elided ({user} user, {assistant} assistant, {tool} tool). \
         Tools used in this span: {tools}. Full detail is preserved in the durable journal.]",
        middle.len()
    );
    Message {
        role: Role::User,
        content: vec![Block::Text(text)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![Block::Text(text.into())],
        }
    }

    #[test]
    fn short_history_is_unchanged() {
        let msgs = vec![msg(Role::User, "a"), msg(Role::Assistant, "b")];
        assert_eq!(project(&msgs, 4).len(), 2);
    }

    #[test]
    fn long_history_is_compacted_keeping_head_and_tail() {
        let mut msgs = vec![msg(Role::User, "TASK")];
        for i in 0..20 {
            msgs.push(msg(Role::Assistant, &format!("step {i}")));
        }
        let projected = project(&msgs, 3);
        // Head + summary + 3 tail = 5.
        assert_eq!(projected.len(), 5);
        assert_eq!(projected[0].text(), "TASK");
        assert!(projected[1].text().contains("elided"));
        assert_eq!(projected.last().unwrap().text(), "step 19");
    }
}
