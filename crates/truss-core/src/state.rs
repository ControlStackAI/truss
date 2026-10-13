//! Durable run state — an append-only JSONL log of every [`AgentEvent`].
//!
//! This is the seam for two enterprise features the rest of the design needs:
//! an **audit trail** (the provenance/evidence behind a "done" verdict) and
//! **resume** (a future loader can replay this log to reconstruct a run). Today
//! it just records; the format is the contract that makes both possible.

use std::path::Path;

use tokio::io::AsyncWriteExt;

use crate::events::AgentEvent;

/// Appends events to a JSONL file, one event per line.
pub struct RunLog {
    file: tokio::fs::File,
}

impl RunLog {
    /// Open (create or append) a log file at `path`.
    pub async fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        Ok(RunLog { file })
    }

    /// Append one event. Serialization failures are dropped silently — the audit
    /// log must never take down a run.
    pub async fn record(&mut self, event: &AgentEvent) {
        if let Ok(mut line) = serde_json::to_string(event) {
            line.push('\n');
            let _ = self.file.write_all(line.as_bytes()).await;
        }
    }
}
