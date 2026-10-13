//! Provider-agnostic conversation types.
//!
//! These are the harness's *internal* representation. Each provider translates
//! to and from its own wire format, so the agent loop never has to care whether
//! it's talking to Anthropic, OpenAI, or a local model.

use serde::{Deserialize, Serialize};

/// Who authored a message. `Tool` carries tool results back to the model;
/// providers map it to whatever role their API expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// A piece of message content. A single message can mix text and tool blocks.
///
/// Multimodal inputs (images, audio) will land here as additional variants; the
/// loop and registry don't care, only providers that advertise the capability do.
// Adjacently tagged (`kind` + `data`): unlike internal tagging, this supports the
// `Text(String)` newtype variant as well as the struct variants — required for
// faithful (de)serialization into snapshots and the durable journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Block {
    /// Plain text.
    Text(String),
    /// The model asking to call a tool (returned by the provider).
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of running a tool (sent back to the provider).
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
}

/// One turn in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    /// Convenience constructor for a plain user-text message.
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![Block::Text(text.into())],
        }
    }

    /// Concatenate all text blocks (ignoring tool blocks).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// The tool-use blocks in this message, if any.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|b| match b {
            Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards journal/snapshot fidelity: every Block variant must round-trip.
    #[test]
    fn blocks_roundtrip_through_serde() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                Block::Text("hello".into()),
                Block::ToolUse {
                    id: "i".into(),
                    name: "n".into(),
                    input: serde_json::json!({ "a": 1 }),
                },
                Block::ToolResult {
                    id: "i".into(),
                    content: "out".into(),
                    is_error: false,
                },
            ],
        }];
        let s = serde_json::to_string(&msgs).expect("serialize");
        let back: Vec<Message> = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].text(), "hello");
        assert_eq!(back[0].content.len(), 3);
    }
}
