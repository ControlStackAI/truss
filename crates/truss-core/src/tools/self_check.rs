use async_trait::async_trait;
use serde_json::{json, Value};

use crate::ctx::Ctx;
use crate::error::ToolError;
use crate::events::AgentEvent;
use crate::provider::ToolDef;
use crate::sandbox::SandboxCommand;
use crate::store::Store;

use super::Tool;

pub struct SelfCheckTool {
    name: &'static str,
    store: Store,
    session_id: String,
    declared_command: Option<String>,
}

impl SelfCheckTool {
    pub fn new(
        name: &'static str,
        store: Store,
        session_id: impl Into<String>,
        declared_command: Option<String>,
    ) -> Self {
        Self {
            name,
            store,
            session_id: session_id.into(),
            declared_command,
        }
    }
}

#[async_trait]
impl Tool for SelfCheckTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.into(),
            description: "Run the project test/self-check command inside the active sandbox. \
                          This records signed self-validation evidence only; it never changes \
                          the harness verdict."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Optional test command. If omitted, the contract's declared check command is used."
                    }
                }
            }),
        }
    }

    async fn call(&self, ctx: &Ctx, input: Value) -> Result<String, ToolError> {
        let command = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| self.declared_command.clone())
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "missing 'command' and no declared self-validation command is configured"
                        .into(),
                )
            })?;

        if let Some(bad) = ctx.policy.command_violation(&command) {
            return Err(ToolError::PermissionDenied(format!(
                "command contains forbidden substring: {bad:?}"
            )));
        }

        let output = ctx
            .sandbox
            .run(SandboxCommand::shell(command.clone()), ctx)
            .await?;
        let summary = summarize(&output.combined_text());
        let exit = output.exit.unwrap_or(-1);
        let workdir_hash = output.workdir_hash.clone();
        let payload = json!({
            "cmd": command,
            "exit": exit,
            "summary": summary,
            "workdir_hash": workdir_hash,
            "evidence_only": true,
            "verdict_unchanged": true,
        });
        self.store
            .append(&self.session_id, "self_validation", &payload)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        ctx.emit(AgentEvent::SelfValidation {
            cmd: payload["cmd"].as_str().unwrap_or("").to_string(),
            exit,
            summary: payload["summary"].as_str().unwrap_or("").to_string(),
            workdir_hash: payload["workdir_hash"].as_str().unwrap_or("").to_string(),
        });

        Ok(format!(
            "self_validation evidence recorded\n$ {}\n[exit {}]\n{}",
            payload["cmd"].as_str().unwrap_or(""),
            exit,
            payload["summary"].as_str().unwrap_or("")
        ))
    }
}

fn summarize(text: &str) -> String {
    const MAX: usize = 600;
    let trimmed = text.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes)", &trimmed[..end], trimmed.len())
}
